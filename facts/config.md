# Configuration Loading — Interface Contract

Owner: main session (root `src/`). Consumers: the `.deb` maintainer scripts,
`install.sh`, and `enr-p2p`'s `Config`.

Design of record: `docs/superpowers/specs/2026-08-11-debconf-first-install-design.md`.
That document explains *why*; this one is what the code must do.

## Problem

"What a first config looks like" was written down in four places and they had
already drifted:

| Source | State |
|---|---|
| `src/main.rs` | Correct. mainnet→9053, testnet→9052. |
| `ergo.toml.example` | Correct. |
| `install.sh` | **Inverted.** mainnet→9052, testnet→9053 — the pre-v0.6.10 values. |
| `deploy/ergo.toml` | **testnet**, testnet seeds, `max_peers = 10`. |

So a `.deb` install lands on a testnet node with no indication anything is
wrong, and anyone who ran `install.sh` and accepted the default got a config
pointing at the wrong API port for their network.

A debconf interview that hard-codes ports and seeds becomes the **fifth** copy
and drifts identically. Layering is what prevents that: the network-dependent
values get exactly one home, and the interview only has to name which network.

## Layout

```
/usr/share/ergo-node-rust/defaults/
    mainnet.toml            package-owned, never edited, replaced on upgrade
    testnet.toml

/etc/ergo-node/conf.d/
    00-defaults.toml        copied from the above by postinst
    50-debconf.toml         generated from debconf answers
    99-local.toml           the operator's. the package never writes it.
```

**Nothing under `conf.d/` is a dpkg conffile.** `00-` and `50-` are
postinst-owned and rewritten freely on `dpkg-reconfigure`; `99-` is the
operator's and dpkg has no opinion about it. This *removes* the
confold/confnew problem rather than managing it — no `ucf`, no three-way
merge, no prompt on upgrade.

Refreshing `00-defaults.toml` from `/usr/share` on every upgrade is what lets
seed-peer changes reach existing operators. Today's list is frozen in a
conffile forever.

## Search path

Unchanged: `./ergo.toml`, `~/.config/ergo-node/ergo.toml`,
`/etc/ergo-node/ergo.toml`, first found wins. A sibling `conf.d/` **next to the
winning file** is then layered on top of it.

⚠ **The loader MUST accept either half alone.** A base file with no `conf.d`
(tarball installs) and a `conf.d` with no base file (the `.deb` end state after
migration) are both valid. Neither is an error, and a `conf.d`-only install must
not fall through to "no config found".

## Merge semantics

Files in `conf.d/` are read in **lexical filename order**, each layered onto the
accumulated result.

| Case | Rule |
|---|---|
| Scalar key | Later file wins, **per key, not per table**. Setting `max_peers` must not drop `seed_peers` from the same table. |
| Array key | Later file **replaces** the array, discarding any `_add` contributions accumulated so far. |
| `<field>_add` array | **Appended** to the accumulated `<field>`, in file order. |

The replace rule is deliberately destructive of prior `_add` entries. Given
`00` setting `seed_peers = [A]`, `50` setting `seed_peers_add = [B]`, and `99`
setting `seed_peers = [C]`, the result is `[C]`. An operator writing a bare
array is stating the complete list; silently retaining an earlier `_add` would
make that statement untrue.

Per-table-key merging is the rule most easily got wrong, because the naive
implementation — insert the later table over the earlier one — passes any test
where the later file sets every key of that table. It fails the moment someone
sets one key. Test the single-key case explicitly.

`_add` applies to **exactly three fields**: `seed_peers`, and `include_ips` /
`exclude_ips` under `[debug.p2p_capture]`. It is a convention for those three,
not a general mechanism applied to every array. `_add` names the operation
rather than the provenance — provenance is already implied by which file the
line is in — and leaves room for a future `_remove`.

## One document, parsed twice

The merged result is a **document**, not a file. It is parsed twice: into
`RootConfig`, and into `enr_p2p::config::Config` for `[proxy]`, `[listen.*]`,
`[outbound]` and `[identity]`.

⚠ **Both parses MUST see the merged document.** `enr_p2p::config::Config::load`
took a path and read the file itself, which would have made the p2p half ignore
every `conf.d` layer while the node half honoured them — a node configured two
different ways in one process. `from_toml_str` exists for this; see
`facts/p2p-node.md` § "Module: `config::Config`".

Do **not** serialise the merged document to a temporary file to satisfy a
path-shaped API. That is the adapter the Interface Integrity rule forbids.

⚠ **`RootConfig` must keep tolerating unknown keys.** It has no
`deny_unknown_fields` deliberately, because this same document carries
`[proxy]`, `[listen.*]`, `[outbound]` and `[identity]` — sections `RootConfig`
does not declare. Denying unknown fields there would reject every real config.
The consequence is that a typo'd key is silently ignored at this level, which is
a known cost of the double-parse, not laxity. Do not "fix" it by adding the
attribute.

## The interview, and what it may write

`50-debconf.toml` is generated from debconf answers. Two screens are
unconditional — **Network** (`select`, default `mainnet`) and **Topics**
(`multiselect`, default nothing selected) — and each selected topic adds its
own questions. An unchecked topic takes defaults silently, so a mainnet
archival node is two screens.

Topics: Interfaces · Node type · Storage · Bootstrap · Mining · Memory · Peers.

Network comes first because it determines the defaults every other topic
computes from. Every question has a default, so `DEBIAN_FRONTEND=noninteractive`
completes without prompting and unattended installs get mainnet.

⚠ **Mining requires `state_type = "utxo"`, and the conflict is removed rather
than reported: if Mining is checked, `digest` is not offered in the Node type
select.** No error path for a combination that means the operator has
misunderstood something more fundamental.

### The Memory topic is one question

⚠ **There are no memory profiles.** An earlier design had
`memory_profile = small | standard | large | custom` plus nine individual knobs.
v0.8.0 derives all nine from the ceiling (see `facts/memory.md`), so a profile
would be a second, worse answer to a question already answered — and a fixed one,
against a floor that **grows with the chain**: `chainIndexBytes` went 9.7 MB at
201k headers to 148 MB at 1.85M. Any profile shipping a number is correct on the
day it is measured and drifts from then on. Do not reintroduce them.

What remains is the one input the system genuinely cannot supply: **how much of
this machine may the node have?** Blank — the default — means derive. A value
writes `memory_budget_mb`, which derivation treats as an explicit statement and
spends at 100%.

The interview should state the floor at this point rather than let the node
refuse later: below 4 GiB warns, below 3 GiB will refuse to start in UTXO mode.
Install time is where an operator can still act on that.

## Migration

`postinst` moves an existing `/etc/ergo-node/ergo.toml` to `conf.d/99-local.toml`
on first upgrade, then the file is dropped from `debian/conffiles`.

This is the **single exception** to "the package never writes `99-local.toml`":
a one-time move, guarded on that file not already existing. Every subsequent
upgrade and every `dpkg-reconfigure` leaves it alone.

⚠ **Without the migration, shipping `00-defaults.toml` layers a network default
on top of every existing operator's hand-tuned file — flipping working mainnet
nodes to testnet on upgrade.** The migration is not tidiness; it is the thing
that stops an upgrade from silently changing which chain a node follows.

Old and new installs converge on the same shape, so the loader carries no
permanent branch for either.

## Testing

1. **Scalar override across files**, and **per-key merge not clobbering
   siblings** in the same table.
2. **Array replace**, and **`_add` append order**, including the
   replace-discards-prior-`_add` case above.
3. **Base file only**; **`conf.d` only**; **neither present**.
4. **Migration** — run postinst against a copy of a real hand-tuned
   `/etc/ergo-node/ergo.toml`; assert it lands at `99-local.toml`
   byte-identical and the merged result is unchanged from pre-upgrade.
5. **Preseed** — `debconf-set-selections` a full answer set, install
   noninteractive, assert the generated config matches.
6. **Noninteractive, no preseed** — assert mainnet, assert the node starts.
7. **`dpkg-reconfigure`** — assert `50-debconf.toml` is rewritten and
   `99-local.toml` is untouched.

## Non-goals

- **Not a general `_add`/`_remove` mechanism.** Three fields, by name.
- **Not an ordering the operator can override.** Lexical filename order is the
  whole mechanism; a file that wants to win names itself later.
- **Not a validation layer.** Merging does not check semantic validity; the
  existing per-field validation runs on the merged result as it does today.
