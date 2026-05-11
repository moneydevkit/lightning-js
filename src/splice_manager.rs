//! Auto-splice manager: a background task that, when the node holds confirmed
//! on-chain funds AND has a usable LSP channel, splices the on-chain balance
//! into the largest such channel. Consolidates liquidity that would otherwise
//! sit idle after a JIT channel closes and a fresh JIT channel opens for the
//! same wallet.
//!
//! Ported from `mdkd::mdk::splice_manager`. The pure decision logic (`decide`,
//! `advance_in_flight`) is identical to the source; the effectful shell is
//! adapted to lightning-js conventions:
//!   * no `MdkClient` — takes `Arc<Node>` + `PublicKey` (LSP) directly;
//!   * no event emission — splice activity is silent, diagnostics go to stderr
//!     with the existing `[lightning-js]` prefix.

#![allow(dead_code)] // Wired in commit 3.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ldk_node::bitcoin::OutPoint;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::{ChannelDetails, Node, NodeError, UserChannelId};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::ResolvedSpliceConfig;

/// Once a splice has been promoted (both sides exchanged `splice_locked` and
/// `ChannelDetails::funding_txo` flips to the new outpoint), we keep the
/// channel out of the candidate pool for this long. ldk-node's background
/// `continuously_sync_wallets` task reruns every ~60s; inside that window
/// `list_balances().spendable_onchain_balance_sats` still reports the
/// pre-splice value (BDK has not yet picked up that the UTXO is spent), so a
/// tick that lands here would happily re-fire on the same UTXO. This grace
/// period bridges the gap between promotion and the next BDK sync.
const BDK_RESYNC_GRACE: Duration = Duration::from_secs(60);

/// Typed splice failure modes. Modeled as an ADT so the apply layer can
/// pattern-match on the failure mode without inspecting strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpliceError {
  /// The target channel exists but is not currently usable (mid-splice, peer
  /// disconnected, mid-monitor-update). Skip this tick and try again.
  ChannelNotUsable,
  /// BDK selection failed because the post-fee result is below the dust
  /// limit. Same retry semantics as `ChannelNotUsable`.
  InsufficientFunds,
  /// Catch-all for any other `NodeError` returned by `Node::splice_in`.
  Rejected,
}

/// `NodeError::InsufficientFunds` is the one case the splice manager treats
/// specially (silent retry). Everything else collapses to `Rejected`.
fn map_splice_error(e: NodeError) -> SpliceError {
  match e {
    NodeError::InsufficientFunds => SpliceError::InsufficientFunds,
    _ => SpliceError::Rejected,
  }
}

pub(crate) fn spawn(
  node: Arc<Node>,
  lsp_pubkey: PublicKey,
  cfg: ResolvedSpliceConfig,
  shutdown: CancellationToken,
  handle: &Handle,
) -> JoinHandle<()> {
  handle.spawn(async move {
    run(node, lsp_pubkey, cfg, shutdown).await;
  })
}

async fn run(
  node: Arc<Node>,
  lsp_pubkey: PublicKey,
  cfg: ResolvedSpliceConfig,
  shutdown: CancellationToken,
) {
  eprintln!(
    "[lightning-js] Splice manager started (poll_interval={}s)",
    cfg.poll_interval.as_secs()
  );
  let mut in_flight: HashMap<u128, InFlight> = HashMap::new();
  loop {
    tokio::select! {
        _ = shutdown.cancelled() => break,
        _ = tokio::time::sleep(cfg.poll_interval) => {
            tick(&node, lsp_pubkey, &mut in_flight);
        }
    }
  }
  eprintln!("[lightning-js] Splice manager stopped");
}

fn tick(node: &Node, lsp_pubkey: PublicKey, in_flight: &mut HashMap<u128, InFlight>) {
  let now = Instant::now();
  let onchain_balance_sats = node.list_balances().spendable_onchain_balance_sats;
  let candidates: Vec<ChannelCandidate> = node.list_channels().iter().map(Into::into).collect();

  advance_in_flight(in_flight, &candidates, now);
  let in_flight_ucids: Vec<u128> = in_flight.keys().copied().collect();

  let decision = decide(
    onchain_balance_sats,
    &candidates,
    &lsp_pubkey,
    &in_flight_ucids,
  );
  if let Some((ucid, initial_funding_txo)) = apply(node, lsp_pubkey, decision) {
    in_flight.insert(
      ucid.0,
      InFlight {
        initial_funding_txo,
        promoted_at: None,
      },
    );
  }
}

/// Advance the in-flight state machine for each tracked splice:
///
/// * If the channel is gone (force-closed mid-splice, etc.), drop the entry —
///   there is nothing to splice into anymore.
/// * If the channel's `funding_txo` still equals the one captured when
///   `splice_in` returned `Ok`, the splice is still negotiating or awaiting
///   the LSP's `splice_locked` (which is gated on
///   `inbound_splice_minimum_depth` confirmations). Keep the entry.
/// * Once `funding_txo` flips, both sides have exchanged `splice_locked` and
///   rust-lightning has swapped the active funding scope. Stamp `promoted_at`
///   on the first observation and drop the entry once `BDK_RESYNC_GRACE` has
///   elapsed, by which point BDK's wallet sync should reflect the spent UTXO.
fn advance_in_flight(
  in_flight: &mut HashMap<u128, InFlight>,
  channels: &[ChannelCandidate],
  now: Instant,
) {
  in_flight.retain(|ucid, inf| {
    let Some(channel) = channels.iter().find(|c| c.user_channel_id.0 == *ucid) else {
      return false;
    };
    if channel.funding_txo == Some(inf.initial_funding_txo) {
      // Splice still negotiating or confirming.
      return true;
    }
    // funding_txo has flipped: splice promoted.
    match inf.promoted_at {
      None => {
        inf.promoted_at = Some(now);
        true
      }
      Some(t) => now.duration_since(t) < BDK_RESYNC_GRACE,
    }
  });
}

/// Pure decision: given a snapshot of the world, what should the splice
/// manager do this tick?
fn decide(
  onchain_balance_sats: u64,
  channels: &[ChannelCandidate],
  lsp_pubkey: &PublicKey,
  in_flight: &[u128],
) -> SpliceDecision {
  if onchain_balance_sats == 0 {
    return SpliceDecision::Skip(SkipReason::NoOnchainBalance);
  }

  // Always splice into the largest LSP channel — concentrating liquidity is
  // the whole point. If that single channel is mid-splice, skip the tick
  // rather than fragmenting funds into a smaller one.
  let Some((channel, funding_txo)) = channels
    .iter()
    .filter(|c| c.counterparty == *lsp_pubkey && c.is_usable)
    .filter_map(|c| c.funding_txo.map(|txo| (c, txo)))
    .max_by_key(|(c, _)| c.channel_value_sats)
  else {
    return SpliceDecision::Skip(SkipReason::NoUsableLspChannel {
      onchain_balance_sats,
    });
  };
  if in_flight.contains(&channel.user_channel_id.0) {
    return SpliceDecision::Skip(SkipReason::SpliceAlreadyInFlight {
      onchain_balance_sats,
    });
  }

  SpliceDecision::Splice(SpliceCandidate {
    user_channel_id: channel.user_channel_id,
    channel_id: channel.channel_id.clone(),
    funding_txo,
  })
}

/// Apply a decision: log, query the maximum splice amount from the node
/// (which runs a dry-run BDK selection at the live channel funding feerate),
/// call `splice_in`. Returns `(user_channel_id, funding_txo)` when a splice
/// was successfully initiated, so the caller can seed the in-flight state
/// machine with the funding outpoint that was active at splice time.
fn apply(
  node: &Node,
  lsp_pubkey: PublicKey,
  decision: SpliceDecision,
) -> Option<(UserChannelId, OutPoint)> {
  match decision {
    SpliceDecision::Skip(SkipReason::NoOnchainBalance) => None,
    SpliceDecision::Skip(SkipReason::NoUsableLspChannel {
      onchain_balance_sats,
    }) => {
      eprintln!(
        "[lightning-js] Splice manager: {onchain_balance_sats} sats on-chain but no usable LSP channel; skipping"
      );
      None
    }
    SpliceDecision::Skip(SkipReason::SpliceAlreadyInFlight {
      onchain_balance_sats,
    }) => {
      eprintln!(
        "[lightning-js] Splice manager: splice already in flight for the eligible LSP channel ({onchain_balance_sats} sats on-chain); skipping until promotion + BDK resync"
      );
      None
    }
    SpliceDecision::Splice(SpliceCandidate {
      user_channel_id,
      channel_id,
      funding_txo,
    }) => {
      let amount_sats = match node.get_max_splice_in_amount(&user_channel_id, lsp_pubkey) {
        Ok(a) => a,
        Err(e) => {
          // Insufficient confirmed UTXOs (post-fee result is dust) is the
          // common case after a small force-close sweep arrives — log and
          // retry next tick.
          eprintln!(
            "[lightning-js] Splice manager: get_max_splice_in_amount failed on channel {channel_id}: {e}; will retry"
          );
          return None;
        }
      };
      eprintln!(
        "[lightning-js] Splice manager: splicing {amount_sats} sats into channel {channel_id}"
      );
      match perform_splice(node, lsp_pubkey, user_channel_id, amount_sats) {
        Ok(()) => Some((user_channel_id, funding_txo)),
        Err(SpliceError::InsufficientFunds) => {
          eprintln!(
            "[lightning-js] Splice manager: insufficient confirmed UTXOs for {amount_sats} sats on channel {channel_id}; will retry"
          );
          None
        }
        Err(SpliceError::ChannelNotUsable) => {
          eprintln!(
            "[lightning-js] Splice manager: channel {channel_id} no longer usable between decide and splice_in; will retry"
          );
          None
        }
        Err(SpliceError::Rejected) => {
          eprintln!("[lightning-js] Splice manager: splice_in rejected on channel {channel_id}");
          None
        }
      }
    }
  }
}

/// Effectful: pre-flight `is_usable` check then delegate to ldk-node. Mirrors
/// `MdkClient::splice_in` from the mdkd source.
fn perform_splice(
  node: &Node,
  lsp_pubkey: PublicKey,
  user_channel_id: UserChannelId,
  amount_sats: u64,
) -> Result<(), SpliceError> {
  let channels = node.list_channels();
  let channel = channels
    .iter()
    .find(|c| c.user_channel_id == user_channel_id)
    .ok_or(SpliceError::ChannelNotUsable)?;
  if !channel.is_usable {
    return Err(SpliceError::ChannelNotUsable);
  }
  node
    .splice_in(&user_channel_id, lsp_pubkey, amount_sats)
    .map_err(map_splice_error)
}

/// Minimal projection of `ChannelDetails` carrying only the fields the splice
/// manager's decision logic needs. Decoupling the pure decision from
/// `ChannelDetails` keeps the unit tests free of LDK type construction.
#[derive(Debug, Clone)]
struct ChannelCandidate {
  user_channel_id: UserChannelId,
  channel_id: String,
  counterparty: PublicKey,
  is_usable: bool,
  funding_txo: Option<OutPoint>,
  channel_value_sats: u64,
}

impl From<&ChannelDetails> for ChannelCandidate {
  fn from(c: &ChannelDetails) -> Self {
    Self {
      user_channel_id: c.user_channel_id,
      channel_id: c.channel_id.to_string(),
      counterparty: c.counterparty_node_id,
      is_usable: c.is_usable,
      funding_txo: c.funding_txo,
      channel_value_sats: c.channel_value_sats,
    }
  }
}

/// State-machine entry tracking a splice the manager initiated. See
/// [`advance_in_flight`] for how `promoted_at` transitions.
#[derive(Debug, Clone)]
struct InFlight {
  initial_funding_txo: OutPoint,
  promoted_at: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum SpliceDecision {
  Skip(SkipReason),
  Splice(SpliceCandidate),
}

#[derive(Debug, PartialEq, Eq)]
enum SkipReason {
  NoOnchainBalance,
  NoUsableLspChannel { onchain_balance_sats: u64 },
  SpliceAlreadyInFlight { onchain_balance_sats: u64 },
}

#[derive(Debug, PartialEq, Eq)]
struct SpliceCandidate {
  user_channel_id: UserChannelId,
  channel_id: String,
  funding_txo: OutPoint,
}

#[cfg(test)]
mod tests {
  use super::*;
  use ldk_node::bitcoin::Txid;
  use ldk_node::bitcoin::hashes::Hash as _;
  use std::str::FromStr;

  fn lsp() -> PublicKey {
    PublicKey::from_str("0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798").unwrap()
  }

  fn other_peer() -> PublicKey {
    PublicKey::from_str("02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5").unwrap()
  }

  /// Build a deterministic `OutPoint` from a single byte so tests can express
  /// "old funding" vs "new funding" without dragging in real txid construction.
  fn txo(seed: u8) -> OutPoint {
    OutPoint {
      txid: Txid::from_byte_array([seed; 32]),
      vout: 0,
    }
  }

  fn candidate(
    counterparty: PublicKey,
    is_usable: bool,
    funding_txo: Option<OutPoint>,
    channel_value_sats: u64,
    ucid: u128,
  ) -> ChannelCandidate {
    ChannelCandidate {
      user_channel_id: UserChannelId(ucid),
      channel_id: format!("ch{ucid}"),
      counterparty,
      is_usable,
      funding_txo,
      channel_value_sats,
    }
  }

  #[test]
  fn skip_when_balance_zero() {
    let lsp = lsp();
    let chans = vec![candidate(lsp, true, Some(txo(1)), 100_000, 1)];
    assert_eq!(
      decide(0, &chans, &lsp, &[]),
      SpliceDecision::Skip(SkipReason::NoOnchainBalance),
    );
  }

  #[test]
  fn skip_when_no_usable_lsp_channel() {
    let lsp = lsp();
    let other = other_peer();
    // Wrong counterparty.
    let chans = vec![candidate(other, true, Some(txo(1)), 100_000, 1)];
    assert_eq!(
      decide(50_000, &chans, &lsp, &[]),
      SpliceDecision::Skip(SkipReason::NoUsableLspChannel {
        onchain_balance_sats: 50_000,
      }),
    );
    // Right peer but not usable.
    let chans = vec![candidate(lsp, false, Some(txo(1)), 100_000, 1)];
    assert_eq!(
      decide(50_000, &chans, &lsp, &[]),
      SpliceDecision::Skip(SkipReason::NoUsableLspChannel {
        onchain_balance_sats: 50_000,
      }),
    );
    // Right peer and usable but no funding_txo (channel still opening).
    let chans = vec![candidate(lsp, true, None, 100_000, 1)];
    assert_eq!(
      decide(50_000, &chans, &lsp, &[]),
      SpliceDecision::Skip(SkipReason::NoUsableLspChannel {
        onchain_balance_sats: 50_000,
      }),
    );
  }

  #[test]
  fn picks_highest_capacity_lsp_channel() {
    let lsp = lsp();
    let other = other_peer();
    let chans = vec![
      // Bigger but wrong peer — ignored.
      candidate(other, true, Some(txo(1)), 1_000_000, 1),
      // Eligible, smaller.
      candidate(lsp, true, Some(txo(2)), 100_000, 2),
      // Eligible, biggest — should win.
      candidate(lsp, true, Some(txo(3)), 500_000, 3),
      // LSP but mid-splice (is_usable false) — ignored.
      candidate(lsp, false, Some(txo(4)), 750_000, 4),
    ];
    let decision = decide(50_000, &chans, &lsp, &[]);
    match decision {
      SpliceDecision::Splice(c) => {
        assert_eq!(c.user_channel_id, UserChannelId(3));
        assert_eq!(c.funding_txo, txo(3));
      }
      other => panic!("expected splice, got {other:?}"),
    }
  }

  #[test]
  fn skip_when_only_eligible_channel_is_in_flight() {
    let lsp = lsp();
    let chans = vec![candidate(lsp, true, Some(txo(5)), 100_000, 5)];
    // The only eligible channel is in-flight.
    assert_eq!(
      decide(50_000, &chans, &lsp, &[5]),
      SpliceDecision::Skip(SkipReason::SpliceAlreadyInFlight {
        onchain_balance_sats: 50_000,
      }),
    );
  }

  #[test]
  fn skip_when_largest_is_in_flight_even_if_smaller_exists() {
    // Concentrating liquidity is the goal: never fall back to a smaller
    // channel just because the largest is mid-splice.
    let lsp = lsp();
    let chans = vec![
      candidate(lsp, true, Some(txo(1)), 500_000, 1),
      candidate(lsp, true, Some(txo(2)), 200_000, 2),
    ];
    assert_eq!(
      decide(50_000, &chans, &lsp, &[1]),
      SpliceDecision::Skip(SkipReason::SpliceAlreadyInFlight {
        onchain_balance_sats: 50_000,
      }),
    );
  }

  fn in_flight(initial_funding_txo: OutPoint, promoted_at: Option<Instant>) -> InFlight {
    InFlight {
      initial_funding_txo,
      promoted_at,
    }
  }

  #[test]
  fn advance_drops_entry_when_channel_is_gone() {
    let mut state: HashMap<u128, InFlight> = HashMap::new();
    state.insert(1, in_flight(txo(1), None));
    advance_in_flight(&mut state, &[], Instant::now());
    assert!(state.is_empty());
  }

  #[test]
  fn advance_keeps_entry_while_funding_unchanged() {
    // Splice is still negotiating / awaiting peer's splice_locked — funding_txo
    // has not flipped yet.
    let lsp = lsp();
    let mut state: HashMap<u128, InFlight> = HashMap::new();
    state.insert(1, in_flight(txo(1), None));
    let chans = vec![candidate(lsp, true, Some(txo(1)), 100_000, 1)];
    advance_in_flight(&mut state, &chans, Instant::now());
    assert!(state.contains_key(&1));
    assert!(state[&1].promoted_at.is_none());
  }

  #[test]
  fn advance_stamps_promotion_then_drops_after_grace() {
    // After both sides splice_locked, funding_txo flips. We hold the entry for
    // BDK_RESYNC_GRACE to let BDK catch up.
    let lsp = lsp();
    let mut state: HashMap<u128, InFlight> = HashMap::new();
    state.insert(1, in_flight(txo(1), None));
    let chans = vec![candidate(lsp, true, Some(txo(2)), 100_000, 1)];

    let t0 = Instant::now();
    advance_in_flight(&mut state, &chans, t0);
    assert_eq!(state[&1].promoted_at, Some(t0));

    // Inside the grace window: still tracked.
    advance_in_flight(
      &mut state,
      &chans,
      t0 + BDK_RESYNC_GRACE - Duration::from_millis(1),
    );
    assert!(state.contains_key(&1));

    // Past the grace window: dropped.
    advance_in_flight(&mut state, &chans, t0 + BDK_RESYNC_GRACE);
    assert!(state.is_empty());
  }

  #[test]
  fn advance_does_not_re_stamp_promotion() {
    // Once promoted_at is set on tick N, later ticks must not reset the clock
    // — otherwise the grace period would never expire.
    let lsp = lsp();
    let mut state: HashMap<u128, InFlight> = HashMap::new();
    let t0 = Instant::now();
    state.insert(1, in_flight(txo(1), Some(t0)));
    let chans = vec![candidate(lsp, true, Some(txo(2)), 100_000, 1)];
    advance_in_flight(&mut state, &chans, t0 + Duration::from_secs(10));
    assert_eq!(state[&1].promoted_at, Some(t0));
  }
}
