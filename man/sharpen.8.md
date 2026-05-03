% SHARPEN(8) ergo-node-rust | System Administration Manual

# NAME

**sharpen** - cut the ergo-node-rust(8) chain tip off above a given height

# SYNOPSIS

**sharpen** \[**--data-dir** *PATH*\] \[**--indexer**\]
\[**--indexer-db** *PATH*\] *HEIGHT*

# DESCRIPTION

**sharpen** rolls the node's UTXO state back to a specified block height
and discards every header and block section above it. After running,
**ergo-node-rust**(8) resumes from *HEIGHT* on next start as if it had
been stopped at exactly that point — no re-sync from genesis required.

The tool is a recovery aid. Typical use cases:

* The node is wedged at the tip with state corruption that survives a
  restart, but blocks several heights back are known good.
* You need to drop a stretch of bad fork-following work and let the
  node re-fetch the canonical chain from peers.
* You ran the indexer addon against blocks that need to be revalidated
  and want to keep node and indexer aligned through the rollback.

The rollback uses the AVL+ undo log retained by **state.redb**, so
**HEIGHT** must be within the configured retention window
(typically the last several hundred blocks). If the target digest is
not in retention, sharpen exits with a clear error and changes nothing.

# OPTIONS

*HEIGHT*
:   Block height to cut at. All data above this height is removed.
    The header at *HEIGHT* itself is retained and becomes the new tip.

**--data-dir** *PATH*
:   Path to the node data directory containing **state.redb** and
    **modifiers.redb**.
    Default: **/var/lib/ergo-node/data**.

**--indexer**
:   Also truncate the ergo-indexer SQLite DB at the default path
    (**/var/lib/ergo-indexer/index.db**). Requires **sqlite3**(1) on
    **PATH**. Mutually compatible with **--indexer-db**.

**--indexer-db** *PATH*
:   Truncate the indexer SQLite DB at *PATH*. Implies **--indexer** —
    pass this flag instead of **--indexer** to override the default
    path.

**-h**, **--help**
:   Print help and exit.

**-V**, **--version**
:   Print version and exit.

# PRECONDITIONS

The node and indexer **MUST be stopped** before running sharpen. Both
**state.redb** and **modifiers.redb** are opened with redb's exclusive
lock; SQLite, by contrast, will happily write under a running indexer
and corrupt it. sharpen opens both databases up-front and bails on
lock failure before performing any work.

# WHAT IT DOES

In order:

1.  Loads the header at *HEIGHT* from **modifiers.redb** and reads its
    AVL state root.
2.  Rolls **state.redb** back to that AVL digest using the undo log.
    Fails with a clear error if the digest is not retained.
3.  Truncates **modifiers.redb**: deletes every best-chain header,
    fork header, block-transactions section, AD-proof section, and
    extension section above *HEIGHT*; cleans the height index.
4.  If **--indexer** or **--indexer-db** was given, runs the indexer's
    rollback cascade against the SQLite DB (boxes, registers, tokens,
    transactions, blocks, indexer_state.height) inside one transaction.

The transformation is atomic per database — if step 3 or step 4 fails
mid-way, the affected database rolls back to its prior state.

# EXAMPLES

Roll the node back to height 1,777,000:

```
sudo systemctl stop ergo-node-rust
sudo -u ergo-node sharpen 1777000
sudo systemctl start ergo-node-rust
```

Roll back node and indexer together (default indexer DB):

```
sudo systemctl stop ergo-node-rust ergo-indexer
sudo -u ergo-node sharpen --indexer 1777000
sudo systemctl start ergo-node-rust ergo-indexer
```

Roll back against a non-default data directory:

```
sharpen --data-dir /opt/ergo/data 1777000
```

# EXIT STATUS

**0**
:   Success.

**non-zero**
:   Failure. Common causes: target *HEIGHT* >= current tip, target AVL
    digest not in retention, exclusive lock held (node still running),
    or **sqlite3**(1) not found when **--indexer** was given.

    On failure, the partially-modified database (if any) has been
    rolled back to its prior state.

# CAVEATS

* sharpen does not back up databases. Operators concerned about
  keeping forensic copies should snapshot **state.redb** and
  **modifiers.redb** before running.
* The indexer rollback shells out to **sqlite3**(1) so the main crate
  doesn't carry a rusqlite dependency. The shipped Debian package
  Suggests sqlite3 only when installing the indexer addon.

# FILES

**/var/lib/ergo-node/data/state.redb**
:   UTXO state and AVL+ tree.

**/var/lib/ergo-node/data/modifiers.redb**
:   Headers, block sections, chain index.

**/var/lib/ergo-indexer/index.db**
:   Default indexer SQLite database (used with **--indexer**).

# SEE ALSO

**ergo-node-rust**(8), **ergo-node-rust.conf**(5), **systemctl**(1),
**sqlite3**(1)
