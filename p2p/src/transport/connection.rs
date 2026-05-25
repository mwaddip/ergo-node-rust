//! Per-connection async read/write loops.
//!
//! # Contract
//! - `Connection::outbound`: performs handshake as initiator (send then receive).
//! - `Connection::inbound`: performs handshake as responder (receive then send).
//! - `Connection::read_frame`: reads the next frame from the stream.
//! - `Connection::write_frame`: writes a frame to the stream.
//! - Invariant: the connection's magic is fixed at construction time.
//! - Invariant: no bytes are lost between handshake and first frame.

use crate::protocol::counters::TrafficCounters;
use crate::transport::frame::{self, Frame};
use crate::transport::handshake::{self, HandshakeConfig, PeerSpec};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{timeout, Duration};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_MAX_SIZE: usize = 8192;

/// An active P2P connection with completed handshake.
pub struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    magic: [u8; 4],
    peer_spec: PeerSpec,
    peer_addr: SocketAddr,
}

impl Connection {
    /// Establish a connection by performing the handshake as initiator (outbound).
    ///
    /// Both directions of the handshake are accounted in `counters` so
    /// operator stats reflect handshake bytes alongside frame traffic.
    pub async fn outbound(
        stream: TcpStream,
        config: &HandshakeConfig,
        counters: &Arc<TrafficCounters>,
    ) -> io::Result<Self> {
        let magic = config.network.magic();
        let peer_addr = stream.peer_addr()?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // Send our handshake
        let hs_bytes = handshake::build(config);
        write_half.write_all(&hs_bytes).await?;
        write_half.flush().await?;
        counters.record_handshake_out(hs_bytes.len() as u64);

        // Read peer's handshake (accumulates TCP segments)
        let (peer_spec, bytes_read) = timeout(HANDSHAKE_TIMEOUT, read_handshake(&mut reader))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Handshake timeout"))??;
        counters.record_handshake_in(bytes_read as u64);

        handshake::validate_peer(&peer_spec, &config.network)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        Ok(Self { reader, writer: write_half, magic, peer_spec, peer_addr })
    }

    /// Accept a connection by performing the handshake as responder (inbound).
    pub async fn inbound(
        stream: TcpStream,
        config: &HandshakeConfig,
        counters: &Arc<TrafficCounters>,
    ) -> io::Result<Self> {
        let magic = config.network.magic();
        let peer_addr = stream.peer_addr()?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // Read peer's handshake first (accumulates TCP segments)
        let (peer_spec, bytes_read) = timeout(HANDSHAKE_TIMEOUT, read_handshake(&mut reader))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Handshake timeout"))??;
        counters.record_handshake_in(bytes_read as u64);

        handshake::validate_peer(&peer_spec, &config.network)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Send our handshake
        let hs_bytes = handshake::build(config);
        write_half.write_all(&hs_bytes).await?;
        write_half.flush().await?;
        counters.record_handshake_out(hs_bytes.len() as u64);

        Ok(Self { reader, writer: write_half, magic, peer_spec, peer_addr })
    }

    /// Read the next message frame.
    pub async fn read_frame(&mut self, blacklist: &crate::blacklist::Blacklist) -> io::Result<Frame> {
        frame::read_frame(&mut self.reader, &self.magic, self.peer_addr, blacklist, None).await
    }

    /// Write a message frame.
    pub async fn write_frame(&mut self, f: &Frame) -> io::Result<()> {
        frame::write_frame(&mut self.writer, &self.magic, f, self.peer_addr, None).await
    }

    pub fn peer_spec(&self) -> &PeerSpec {
        &self.peer_spec
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    pub fn split(self) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf, [u8; 4], PeerSpec, SocketAddr) {
        (self.reader, self.writer, self.magic, self.peer_spec, self.peer_addr)
    }
}

/// Read a handshake by accumulating TCP segments via the BufReader.
///
/// TCP may deliver the handshake across multiple segments, especially over
/// the internet. BufReader::fill_buf() only returns cached data without
/// re-reading if there's unconsumed data. We use read() into a local Vec
/// to accumulate, then on success, we "unread" the excess bytes by putting
/// them back into the BufReader's stream via a workaround: we DON'T use
/// the accumulated bytes directly — instead we consume only the handshake
/// portion from the BufReader and leave excess for frame reading.
///
/// Strategy: read byte-by-byte from the BufReader until parse succeeds.
/// This is slow but handshakes are small (~200 bytes) and happen once per
/// connection. The BufReader retains any excess bytes internally.
///
/// Returns the parsed spec and the number of bytes consumed from the
/// stream so callers can credit handshake traffic counters.
async fn read_handshake(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<(PeerSpec, usize)> {
    let mut buf = Vec::with_capacity(256);

    loop {
        // Read one byte at a time from the BufReader.
        // This lets the BufReader manage its internal buffer — excess bytes
        // (from the first P2P frame) stay in the buffer for read_frame.
        let mut byte = [0u8; 1];
        match reader.read(&mut byte).await? {
            0 => {
                if buf.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, "Empty handshake"));
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Incomplete handshake"));
            }
            _ => buf.push(byte[0]),
        }

        if buf.len() > HANDSHAKE_MAX_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Handshake exceeds maximum size",
            ));
        }

        match handshake::parse(&buf) {
            Ok(spec) => {
                tracing::debug!(
                    handshake_bytes = buf.len(),
                    "handshake parsed successfully"
                );
                return Ok((spec, buf.len()));
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => continue,
            Err(e) => return Err(e),
        }
    }
}
