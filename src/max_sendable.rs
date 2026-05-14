//! Estimator for the largest amount that can be sent over Lightning
//! out of mdk's LSP channel(s), with routing fees subtracted.
//!
//! v0 is destination-agnostic: it subtracts a configurable percentage
//! buffer (default 1%, 10-sat floor) from the sum of usable LSP
//! channels' `next_outbound_htlc_limit_msat`. v1 will replace the
//! buffer with a real `Router::find_route` + per-hop fee inversion.
//! The [`compute_estimate`] function is the seam — the accessor that
//! calls it stays put across v0→v1.

use ldk_node::ChannelDetails;
use ldk_node::bitcoin::secp256k1::PublicKey;

/// User-tunable buffer applied to the raw outbound liquidity to
/// reserve headroom for routing fees.
#[derive(Debug, Clone)]
pub(crate) struct MaxSendableConfig {
  /// Percentage buffer in basis points (1 bps = 0.01 %). Default: 100 (1 %).
  pub fee_buffer_bps: u16,
  /// Absolute lower bound on the buffer, in sats. Default: 10.
  /// Whichever of the percentage and the floor is larger wins —
  /// keeps small-balance estimates honest about base fees.
  pub fee_buffer_floor_sats: u64,
}

impl Default for MaxSendableConfig {
  fn default() -> Self {
    Self {
      fee_buffer_bps: 100,
      fee_buffer_floor_sats: 10,
    }
  }
}

/// A best-effort estimate of how much can flow out over Lightning
/// right now, alongside the fee headroom the estimate carved out.
#[derive(Debug, Clone)]
pub(crate) struct MaxSendableEstimate {
  /// Amount to surface to the payer as "max sendable", in msat.
  /// Zero when the balance is fully consumed by the buffer (dust).
  pub amount_msat: u64,
  /// The buffer subtracted from the raw outbound liquidity to reach
  /// `amount_msat`. Doubles as a hint for `max_total_routing_fee_msat`.
  pub fee_budget_msat: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MaxSendableError {
  /// No usable LSP channel exists yet — the node is still booting,
  /// the channel is opening, or it was force-closed. Distinct from
  /// "balance is dust" (which returns `Ok(amount_msat: 0)`).
  NoUsableChannel,
}

/// Minimal projection of `ldk_node::ChannelDetails` carrying only the
/// fields [`compute_estimate`] looks at.
#[derive(Debug, Clone)]
pub(crate) struct ChannelSnapshot {
  pub counterparty: PublicKey,
  pub is_usable: bool,
  pub next_outbound_htlc_limit_msat: u64,
}

impl From<&ChannelDetails> for ChannelSnapshot {
  fn from(c: &ChannelDetails) -> Self {
    Self {
      counterparty: c.counterparty_node_id,
      is_usable: c.is_usable,
      next_outbound_htlc_limit_msat: c.next_outbound_htlc_limit_msat,
    }
  }
}

/// Given a snapshot of channels, the LSP pubkey, and a
/// buffer config, return the estimate.
///
/// `Err(NoUsableChannel)` is returned only when no channel matches
/// `counterparty == lsp_pubkey && is_usable`. A dust-level balance
/// where the buffer eats everything yields `Ok(amount_msat: 0)` — the
/// UI distinguishes "0 sats sendable" from "no channel yet".
pub(crate) fn compute_estimate(
  channels: &[ChannelSnapshot],
  lsp_pubkey: &PublicKey,
  cfg: &MaxSendableConfig,
) -> Result<MaxSendableEstimate, MaxSendableError> {
  // `Option<u64>` accumulator distinguishes "no channel matched"
  // (None → NoUsableChannel) from "channel(s) matched, sum is 0"
  // (Some(0) → Ok with dust semantics).
  let balance_msat = channels
    .iter()
    .filter(|c| c.counterparty == *lsp_pubkey && c.is_usable)
    .fold(None::<u64>, |acc, c| {
      Some(
        acc
          .unwrap_or(0)
          .saturating_add(c.next_outbound_htlc_limit_msat),
      )
    })
    .ok_or(MaxSendableError::NoUsableChannel)?;

  // u128 intermediate dodges overflow at the percentage step. ppm
  // basis-points × u64 msat fits in u128 trivially, and the divide
  // brings it back into u64 range.
  let pct_buffer = ((balance_msat as u128) * (cfg.fee_buffer_bps as u128) / 10_000) as u64;
  let floor_buffer = cfg.fee_buffer_floor_sats.saturating_mul(1_000);
  let buffer_msat = pct_buffer.max(floor_buffer);

  Ok(MaxSendableEstimate {
    amount_msat: balance_msat.saturating_sub(buffer_msat),
    fee_budget_msat: buffer_msat,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::str::FromStr;

  fn lsp() -> PublicKey {
    PublicKey::from_str("0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798").unwrap()
  }

  fn other_peer() -> PublicKey {
    PublicKey::from_str("02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5").unwrap()
  }

  fn snap(counterparty: PublicKey, is_usable: bool, limit_msat: u64) -> ChannelSnapshot {
    ChannelSnapshot {
      counterparty,
      is_usable,
      next_outbound_htlc_limit_msat: limit_msat,
    }
  }

  #[test]
  fn no_usable_channel_when_empty() {
    let lsp = lsp();
    let res = compute_estimate(&[], &lsp, &MaxSendableConfig::default());
    assert!(matches!(res, Err(MaxSendableError::NoUsableChannel)));
  }

  #[test]
  fn no_usable_channel_when_only_other_counterparty() {
    let lsp = lsp();
    let chans = [snap(other_peer(), true, 100_000_000)];
    let res = compute_estimate(&chans, &lsp, &MaxSendableConfig::default());
    assert!(matches!(res, Err(MaxSendableError::NoUsableChannel)));
  }

  #[test]
  fn no_usable_channel_when_lsp_channel_unusable() {
    // Channel exists with the LSP but is mid-open or mid-splice
    // — explicitly distinct from "balance is zero".
    let lsp = lsp();
    let chans = [snap(lsp, false, 100_000_000)];
    let res = compute_estimate(&chans, &lsp, &MaxSendableConfig::default());
    assert!(matches!(res, Err(MaxSendableError::NoUsableChannel)));
  }

  #[test]
  fn dust_balance_below_floor_returns_zero() {
    // 5 sats of outbound. Floor buffer is 10 sats → buffer wins,
    // amount saturates to zero. The estimate is "you have
    // liquidity, but it can't cover even the floor fee" — not an
    // error.
    let lsp = lsp();
    let chans = [snap(lsp, true, 5_000)]; // 5 sats
    let est = compute_estimate(&chans, &lsp, &MaxSendableConfig::default()).unwrap();
    assert_eq!(est.amount_msat, 0);
    assert_eq!(est.fee_budget_msat, 10_000); // 10-sat floor
  }

  #[test]
  fn balance_exactly_equals_buffer_returns_zero() {
    // 10 sats balance, 10 sat floor → amount = 0 exactly,
    // fee_budget = 10_000 msat.
    let lsp = lsp();
    let chans = [snap(lsp, true, 10_000)];
    let est = compute_estimate(&chans, &lsp, &MaxSendableConfig::default()).unwrap();
    assert_eq!(est.amount_msat, 0);
    assert_eq!(est.fee_budget_msat, 10_000);
  }

  #[test]
  fn normal_case_percentage_buffer_dominates() {
    // 100k sats × 1% = 1000 sats > 10-sat floor → percentage wins.
    let lsp = lsp();
    let chans = [snap(lsp, true, 100_000_000)]; // 100k sats
    let est = compute_estimate(&chans, &lsp, &MaxSendableConfig::default()).unwrap();
    assert_eq!(est.fee_budget_msat, 1_000_000); // 1000 sats
    assert_eq!(est.amount_msat, 99_000_000); // 99k sats
  }

  #[test]
  fn normal_case_floor_buffer_dominates() {
    // 500 sats × 1% = 5 sats < 10-sat floor → floor wins.
    let lsp = lsp();
    let chans = [snap(lsp, true, 500_000)]; // 500 sats
    let est = compute_estimate(&chans, &lsp, &MaxSendableConfig::default()).unwrap();
    assert_eq!(est.fee_budget_msat, 10_000); // 10-sat floor
    assert_eq!(est.amount_msat, 490_000); // 490 sats
  }

  #[test]
  fn two_usable_lsp_channels_sum() {
    // mdk does not bake in a single-channel assumption — if two
    // usable LSP channels exist (rare but legal), their
    // `next_outbound_htlc_limit_msat` values sum.
    let lsp = lsp();
    let chans = [
      snap(lsp, true, 50_000_000), // 50k sats
      snap(lsp, true, 30_000_000), // 30k sats
    ];
    let est = compute_estimate(&chans, &lsp, &MaxSendableConfig::default()).unwrap();
    assert_eq!(est.fee_budget_msat, 800_000); // 1% of 80k sats
    assert_eq!(est.amount_msat, 79_200_000);
  }

  #[test]
  fn mixed_channels_only_usable_lsp_contributes() {
    // Only the usable LSP channel counts: non-LSP and
    // unusable-LSP entries are filtered out.
    let lsp = lsp();
    let other = other_peer();
    let chans = [
      snap(lsp, true, 10_000_000),   // counts
      snap(other, true, 50_000_000), // wrong peer
      snap(lsp, false, 100_000_000), // mid-open/splice
    ];
    let est = compute_estimate(&chans, &lsp, &MaxSendableConfig::default()).unwrap();
    assert_eq!(est.fee_budget_msat, 100_000); // 1% of 10k sats
    assert_eq!(est.amount_msat, 9_900_000);
  }

  #[test]
  fn overrides_take_effect() {
    // Custom bps and floor flow through end-to-end. 200 bps = 2%.
    let lsp = lsp();
    let chans = [snap(lsp, true, 1_000_000_000)]; // 1M sats
    let cfg = MaxSendableConfig {
      fee_buffer_bps: 200,
      fee_buffer_floor_sats: 50,
    };
    let est = compute_estimate(&chans, &lsp, &cfg).unwrap();
    assert_eq!(est.fee_budget_msat, 20_000_000); // 2% of 1M sats = 20k sats
    assert_eq!(est.amount_msat, 980_000_000);
  }
}
