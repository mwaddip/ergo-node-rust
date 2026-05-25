#[cfg(test)]
mod tests {
    use crate::migrate::*;
    use anyhow::Result;
    use async_trait::async_trait;

    /// Fake backend that stores cursor in memory, for testing the
    /// cursor read/write contract independent of any real DB.
    struct FakeBackend {
        schema: Option<u32>,
        cursor: Option<Cursor>,
    }

    #[async_trait]
    impl Backend for FakeBackend {
        async fn schema_version(&mut self) -> Result<Option<u32>> {
            Ok(self.schema)
        }
        async fn init_schema(&mut self, v: u32) -> Result<()> {
            self.schema = Some(v);
            Ok(())
        }
        async fn read_cursor(&mut self) -> Result<Option<Cursor>> {
            Ok(self.cursor.clone())
        }
        async fn write_cursor(&mut self, c: &Cursor) -> Result<()> {
            self.cursor = Some(c.clone());
            Ok(())
        }
        async fn max_height(&mut self) -> Result<Option<u32>> {
            unimplemented!()
        }
        async fn read_block_data(&mut self, _: u32) -> Result<BlockData> {
            unimplemented!()
        }
        async fn apply_block(&mut self, _: &BlockData, _: &Cursor) -> Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn cursor_roundtrip() {
        let mut b = FakeBackend { schema: None, cursor: None };
        assert!(b.read_cursor().await.unwrap().is_none());
        let c = Cursor {
            last_height: 1000,
            source_url: "sqlite:///x.db".into(),
            source_fingerprint: [0x42; 32],
        };
        b.write_cursor(&c).await.unwrap();
        assert_eq!(b.read_cursor().await.unwrap().as_ref(), Some(&c));
    }
}
