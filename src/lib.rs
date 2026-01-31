#![deny(clippy::all)]

use std::{
  collections::{HashMap, HashSet},
  convert::TryFrom,
  fmt::Write,
  str::FromStr,
  sync::{
    Arc, OnceLock, RwLock,
    atomic::{AtomicU8, Ordering},
  },
  time::{Duration, Instant},
};

use bitcoin_payment_instructions::{
  PaymentInstructions, PaymentMethod, amount::Amount as InstructionAmount,
  http_resolver::HTTPHrnResolver,
};
use napi::{
  Env, JsFunction, Status,
  threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};

use ldk_node::logger::{LogLevel, LogRecord, LogWriter};
use ldk_node::{
  Builder, Event, Node,
  bip39::Mnemonic,
  bitcoin::{
    Network,
    hashes::{Hash, sha256},
    secp256k1::PublicKey,
  },
  config::Config,
  generate_entropy_mnemonic,
  lightning::ln::channelmanager::PaymentId,
  lightning::{ln::msgs::SocketAddress, offers::offer::Offer, util::scid_utils},
  lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description},
  lightning_types::payment::PaymentHash,
};
use tokio::runtime::Runtime;

#[macro_use]
extern crate napi_derive;

/// Polling interval for event loops and state checks.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Max time to wait for channels to become usable after sync.
const CHANNEL_USABLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Internal enum representing a resolved payment target with amount
enum PaymentTarget {
  Bolt11(Bolt11Invoice, u64), // invoice + amount_msat
  Bolt12(Box<Offer>, u64),    // offer + amount_msat
}

impl PaymentTarget {
  fn amount_msat(&self) -> u64 {
    match self {
      PaymentTarget::Bolt11(_, amount) | PaymentTarget::Bolt12(_, amount) => *amount,
    }
  }
}

static GLOBAL_LOGGER: OnceLock<Arc<JsLogger>> = OnceLock::new();

fn logger_instance() -> &'static Arc<JsLogger> {
  GLOBAL_LOGGER.get_or_init(|| Arc::new(JsLogger::new()))
}

fn level_to_str(level: LogLevel) -> &'static str {
  match level {
    LogLevel::Gossip => "GOSSIP",
    LogLevel::Trace => "TRACE",
    LogLevel::Debug => "DEBUG",
    LogLevel::Info => "INFO",
    LogLevel::Warn => "WARN",
    LogLevel::Error => "ERROR",
  }
}

fn level_to_rank(level: LogLevel) -> u8 {
  level as u8
}

fn parse_level(level: &str) -> Option<LogLevel> {
  match level.to_ascii_uppercase().as_str() {
    "GOSSIP" => Some(LogLevel::Gossip),
    "TRACE" => Some(LogLevel::Trace),
    "DEBUG" => Some(LogLevel::Debug),
    "INFO" => Some(LogLevel::Info),
    "WARN" | "WARNING" => Some(LogLevel::Warn),
    "ERROR" => Some(LogLevel::Error),
    _ => None,
  }
}

#[derive(Clone)]
struct LogMessage {
  level: LogLevel,
  module_path: String,
  line: u32,
  message: String,
}

struct JsLogger {
  listener: RwLock<Option<ThreadsafeFunction<LogMessage>>>,
  min_level: AtomicU8,
}

impl JsLogger {
  fn new() -> Self {
    Self {
      listener: RwLock::new(None),
      min_level: AtomicU8::new(level_to_rank(LogLevel::Info)),
    }
  }

  fn set_listener(
    &self,
    env: Env,
    callback: Option<JsFunction>,
    min_level: Option<String>,
  ) -> napi::Result<()> {
    if let Some(level_str) = min_level {
      if let Some(level) = parse_level(&level_str) {
        self
          .min_level
          .store(level_to_rank(level), Ordering::Relaxed);
      } else {
        return Err(napi::Error::new(
          Status::InvalidArg,
          format!("Unknown log level '{level_str}'"),
        ));
      }
    }

    let mut guard = self.listener.write().unwrap();
    *guard = match callback {
      Some(cb) => {
        let mut tsfn = cb.create_threadsafe_function(0, |ctx| {
          let LogMessage {
            level,
            module_path,
            line,
            message,
          } = ctx.value;
          let env = ctx.env;
          let mut js_obj = env.create_object()?;
          let level_str = level_to_str(level);
          js_obj.set_named_property("level", env.create_string(level_str)?)?;
          js_obj.set_named_property("modulePath", env.create_string(&module_path)?)?;
          js_obj.set_named_property("line", env.create_uint32(line)?)?;
          js_obj.set_named_property("message", env.create_string(&message)?)?;
          Ok(vec![js_obj.into_unknown()])
        })?;
        tsfn.unref(&env)?;
        Some(tsfn)
      }
      None => None,
    };

    Ok(())
  }

  fn dispatch(&self, message: LogMessage) {
    if level_to_rank(message.level) < self.min_level.load(Ordering::Relaxed) {
      return;
    }

    let maybe_tsfn = self.listener.read().unwrap().clone();
    if let Some(tsfn) = maybe_tsfn {
      let status = tsfn.call(Ok(message.clone()), ThreadsafeFunctionCallMode::NonBlocking);
      if status != Status::Ok {
        eprintln!(
          "[ldk-node {} {}:{}] {}",
          level_to_str(message.level),
          message.module_path,
          message.line,
          message.message,
        );
      }
    } else {
      eprintln!(
        "[ldk-node {} {}:{}] {}",
        level_to_str(message.level),
        message.module_path,
        message.line,
        message.message,
      );
    }
  }
}

impl LogWriter for JsLogger {
  fn log(&self, record: LogRecord<'_>) {
    let message = format!("{}", record.args);
    let payload = LogMessage {
      level: record.level,
      module_path: record.module_path.to_string(),
      line: record.line,
      message,
    };
    self.dispatch(payload);
  }
}

#[napi]
pub fn set_log_listener(
  env: Env,
  callback: Option<JsFunction>,
  min_level: Option<String>,
) -> napi::Result<()> {
  logger_instance().set_listener(env, callback, min_level)
}

#[napi]
pub fn generate_mnemonic() -> String {
  generate_entropy_mnemonic(None).to_string()
}

fn derive_vss_identifier(mnemonic: &Mnemonic) -> String {
  let mnemonic_phrase = mnemonic.to_string();
  sha256::Hash::hash(mnemonic_phrase.as_bytes()).to_string()
}

#[napi(object)]
pub struct MdkNodeOptions {
  pub network: String,
  pub mdk_api_key: String,
  pub vss_url: String,
  pub esplora_url: String,
  pub rgs_url: String,
  pub mnemonic: String,
  pub lsp_node_id: String,
  pub lsp_address: String,
}

#[napi(object)]
pub struct PaymentMetadata {
  pub bolt11: String,
  pub payment_hash: String,
  pub expires_at: i64,
  pub scid: String,
}

#[napi(object)]
pub struct ReceivedPayment {
  pub payment_hash: String,
  pub amount: i64,
}

#[napi(object)]
pub struct PaymentEvent {
  pub event_type: PaymentEventType,
  pub payment_hash: String,
  pub amount_msat: Option<i64>,
  pub reason: Option<String>,
}

#[napi]
pub enum PaymentEventType {
  Claimable,
  Received,
  Failed,
}

#[napi(object)]
pub struct NodeChannel {
  pub channel_id: String,
  pub counterparty_node_id: String,
  pub short_channel_id: Option<String>,
  pub inbound_capacity_msat: i64,
  pub outbound_capacity_msat: i64,
  pub is_channel_ready: bool,
  pub is_usable: bool,
  pub is_public: bool,
}

#[napi]
pub struct MdkNode {
  node: Node,
  network: Network,
}

#[napi]
impl MdkNode {
  #[napi(constructor)]
  pub fn new(options: MdkNodeOptions) -> napi::Result<Self> {
    let network = match options.network.as_str() {
      "mainnet" => Network::Bitcoin,
      "testnet" => Network::Testnet,
      "signet" => Network::Signet,
      "regtest" => Network::Regtest,
      _ => Network::Signet,
    };

    let mnemonic = Mnemonic::from_str(&options.mnemonic).unwrap();
    let vss_identifier = derive_vss_identifier(&mnemonic);
    let lsp_node_id = PublicKey::from_str(&options.lsp_node_id).unwrap();
    let lsp_address = SocketAddress::from_str(&options.lsp_address).unwrap();

    // Create a config with anchor channels disabled
    // This prevents the node from advertising anchor channel support,
    // so the LSP will only attempt to open non-anchor channels
    let config = Config {
      anchor_channels_config: None,
      ..Config::default()
    };

    let mut builder = Builder::from_config(config);
    builder.set_network(network);
    builder.set_chain_source_esplora(options.esplora_url, None);
    builder.set_gossip_source_rgs(options.rgs_url);
    builder.set_entropy_bip39_mnemonic(mnemonic, None);
    let logger_arc = Arc::clone(logger_instance());
    let logger: Arc<dyn LogWriter> = logger_arc;
    builder.set_custom_logger(logger);
    builder.set_liquidity_source_lsps4(lsp_node_id, lsp_address);

    let vss_headers = HashMap::from([(
      "Authorization".to_string(),
      format!("Bearer {}", options.mdk_api_key),
    )]);

    let node = builder
      .build_with_vss_store_and_fixed_headers(options.vss_url, vss_identifier, vss_headers)
      .map_err(|err| napi::Error::from_reason(err.to_string()))?;

    Ok(Self { node, network })
  }

  #[napi]
  pub fn get_node_id(&self) -> String {
    self.node.node_id().to_string()
  }

  #[napi]
  pub fn start(&self) {
    if let Err(err) = self.node.start() {
      eprintln!("[lightning-js] Failed to start node via start(): {err}");
      panic!("failed to start node: {err}");
    }
  }

  #[napi]
  pub fn stop(&self) {
    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node via stop(): {err}");
      panic!("failed to stop node: {err}");
    }
  }

  /// Start the node and sync wallets. Call once before polling for events.
  #[napi]
  pub fn start_receiving(&self) -> napi::Result<()> {
    self.node.start().map_err(|e| {
      eprintln!("[lightning-js] Failed to start node in start_receiving: {e}");
      napi::Error::from_reason(format!("Failed to start: {e}"))
    })?;

    self.node.sync_wallets().map_err(|e| {
      eprintln!("[lightning-js] Failed to sync wallets in start_receiving: {e}");
      let _ = self.node.stop();
      napi::Error::from_reason(format!("Failed to sync: {e}"))
    })
  }

  /// Get the next payment event without ACKing it.
  /// Returns None if no events are available.
  /// Call ack_event() after successfully handling the event.
  #[napi]
  pub fn next_event(&self) -> Option<PaymentEvent> {
    loop {
      match self.node.next_event() {
        Some(event) => {
          let payment_event = match &event {
            Event::PaymentClaimable {
              payment_hash,
              claimable_amount_msat,
              ..
            } => Some(PaymentEvent {
              event_type: PaymentEventType::Claimable,
              payment_hash: bytes_to_hex(&payment_hash.0),
              amount_msat: Some(*claimable_amount_msat as i64),
              reason: None,
            }),
            Event::PaymentReceived {
              payment_hash,
              amount_msat,
              ..
            } => Some(PaymentEvent {
              event_type: PaymentEventType::Received,
              payment_hash: bytes_to_hex(&payment_hash.0),
              amount_msat: Some(*amount_msat as i64),
              reason: None,
            }),
            Event::PaymentFailed {
              payment_hash,
              reason,
              ..
            } => payment_hash.map(|h| PaymentEvent {
              event_type: PaymentEventType::Failed,
              payment_hash: bytes_to_hex(&h.0),
              amount_msat: None,
              reason: reason.map(|r| format!("{r:?}")),
            }),
            _ => None,
          };

          // If this is a payment event we care about, return it (without ACKing)
          if payment_event.is_some() {
            return payment_event;
          }

          // For non-payment events, ACK and continue to next event
          // Potentially problematic if a payout is happening at the same time
          // but this is the existing behavior.
          let _ = self.node.event_handled();
        }
        None => return None,
      }
    }
  }

  /// ACK the current event after successfully handling it.
  /// Must be called after next_event() returns an event, before calling next_event() again.
  #[napi]
  pub fn ack_event(&self) -> napi::Result<()> {
    self
      .node
      .event_handled()
      .map_err(|e| napi::Error::from_reason(format!("Failed to ack event: {e}")))
  }

  /// Stop the node. Call when done polling.
  #[napi]
  pub fn stop_receiving(&self) -> napi::Result<()> {
    self
      .node
      .stop()
      .map_err(|e| napi::Error::from_reason(format!("Failed to stop: {e}")))
  }

  #[napi]
  pub fn sync_wallets(&self) {
    if let Err(err) = self.node.start() {
      eprintln!("[lightning-js] Failed to start node via start(): {err}");
      panic!("failed to start node: {err}");
    }

    if let Err(err) = self.node.sync_wallets() {
      eprintln!("[lightning-js] Failed to sync wallets: {err}");
      panic!("failed to sync wallets: {err}");
    }

    let channels = self.node.list_channels();
    for c in channels {
      eprintln!(
        "{} accept_underpaying_htlcs={:?}",
        c.channel_id, c.config.accept_underpaying_htlcs
      );
    }

    let peers = self.node.list_peers();
    for peer in peers {
      eprintln!(
        "[lightning-js] Peer node_id={} address={} persisted={} connected={}",
        peer.node_id, peer.address, peer.is_persisted, peer.is_connected
      );
    }

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node via stop(): {err}");
      panic!("failed to stop node: {err}");
    }
  }

  #[napi]
  pub fn get_balance(&self) -> i64 {
    if let Err(err) = self.node.start() {
      eprintln!("[lightning-js] Failed to start node via get_balance: {err}");
      panic!("failed to start node: {err}");
    }

    if let Err(err) = self.node.sync_wallets() {
      eprintln!("[lightning-js] Failed to sync wallets in get_balance: {err}");
      if let Err(stop_err) = self.node.stop() {
        eprintln!(
          "[lightning-js] Failed to stop node after sync_wallets error in get_balance: {stop_err}"
        );
      }
      panic!("failed to sync wallets: {err}");
    }

    let balance = self.get_balance_impl();

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node via stop() in get_balance: {err}");
      panic!("failed to stop node: {err}");
    }

    balance
  }

  /// Get balance without starting/stopping the node.
  /// Use this when the node is already running via start_receiving().
  #[napi]
  pub fn get_balance_while_running(&self) -> i64 {
    self.get_balance_impl()
  }

  fn get_balance_impl(&self) -> i64 {
    let total_outbound_msat = self
      .node
      .list_channels()
      .into_iter()
      .fold(0u64, |acc, channel| {
        acc.saturating_add(channel.outbound_capacity_msat)
      });

    u64_to_i64(total_outbound_msat / 1_000)
  }

  #[napi]
  pub fn list_channels(&self) -> Vec<NodeChannel> {
    self
      .node
      .list_channels()
      .into_iter()
      .map(|details| {
        let channel_id = bytes_to_hex(&details.channel_id.0);
        let counterparty_node_id = details.counterparty_node_id.to_string();
        let short_channel_id = details.short_channel_id.map(scid_to_string);
        let inbound_capacity_msat = u64_to_i64(details.inbound_capacity_msat);
        let outbound_capacity_msat = u64_to_i64(details.outbound_capacity_msat);
        let is_channel_ready = details.is_channel_ready;
        let is_usable = details.is_usable;
        let is_public = details.is_announced;

        NodeChannel {
          channel_id,
          counterparty_node_id,
          short_channel_id,
          inbound_capacity_msat,
          outbound_capacity_msat,
          is_channel_ready,
          is_usable,
          is_public,
        }
      })
      .collect()
  }

  /// Manually sync the RGS snapshot.
  ///
  /// If `do_full_sync` is true, the RGS snapshot will be updated from scratch. Otherwise, the
  /// snapshot will be updated from the last known sync point.
  #[napi]
  pub fn sync_rgs(&self, do_full_sync: bool) -> Result<u32, napi::Error> {
    let rt = tokio::runtime::Runtime::new()
      .map_err(|e| napi::Error::from_reason(format!("Failed to create runtime: {}", e)))?;
    rt.block_on(async {
      self
        .node
        .sync_rgs(do_full_sync)
        .await
        .map_err(|e| napi::Error::from_reason(format!("Failed to sync RGS: {}", e)))
    })
  }

  #[napi]
  pub fn receive_payment(
    &self,
    min_threshold_ms: i64,
    quiet_threshold_ms: i64,
  ) -> Vec<ReceivedPayment> {
    // Hard timeout to prevent infinite wait if claims get stuck (60 seconds)
    const HARD_TIMEOUT_MS: i64 = 60_000;

    let mut received_payments = vec![];
    let mut pending_claims: HashSet<PaymentHash> = HashSet::new();

    if let Err(err) = self.node.start() {
      eprintln!("[lightning-js] Failed to start node in receive_payment: {err}");
      return received_payments;
    }

    if let Err(err) = self.node.sync_wallets() {
      eprintln!("[lightning-js] Failed to sync wallets: {err}");
      panic!("failed to sync wallets: {err}");
    }

    let start_sync_at = std::time::Instant::now();
    let mut last_event_time = start_sync_at;

    loop {
      let now = std::time::Instant::now();

      let total_time_elapsed = now.duration_since(start_sync_at).as_millis() as i64;
      let quiet_time_elapsed = now.duration_since(last_event_time).as_millis() as i64;

      let can_exit = pending_claims.is_empty()
        && total_time_elapsed >= min_threshold_ms
        && quiet_time_elapsed >= quiet_threshold_ms;

      if can_exit {
        break;
      }

      if total_time_elapsed >= HARD_TIMEOUT_MS {
        if !pending_claims.is_empty() {
          eprintln!(
            "[lightning-js] WARNING: Exiting receive_payment with {} pending claims after hard timeout ({}ms): {:?}",
            pending_claims.len(),
            HARD_TIMEOUT_MS,
            pending_claims
              .iter()
              .map(|h| bytes_to_hex(&h.0))
              .collect::<Vec<_>>()
          );
        }
        break;
      }

      if let Some(event) = self.node.next_event() {
        eprintln!("[lightning-js] Event: {event:?}");

        match &event {
          Event::PaymentFailed {
            payment_id,
            payment_hash,
            reason,
          } => {
            let payment_id_hex = payment_id
              .as_ref()
              .map(|id| bytes_to_hex(&id.0))
              .unwrap_or_else(|| "None".to_string());
            let payment_hash_hex = payment_hash
              .as_ref()
              .map(|hash| bytes_to_hex(&hash.0))
              .unwrap_or_else(|| "None".to_string());
            let reason_str = reason
              .as_ref()
              .map(|r| format!("{r:?}"))
              .unwrap_or_else(|| "Unknown".to_string());

            eprintln!(
              "[lightning-js] PaymentFailed payment_id={payment_id_hex} payment_hash={payment_hash_hex} reason={reason_str}",
            );
            if let Some(hash) = payment_hash {
              pending_claims.remove(hash);
            }
          }
          Event::PaymentClaimable {
            payment_hash,
            claimable_amount_msat,
            claim_deadline,
            ..
          } => {
            let payment_hash_hex = bytes_to_hex(&payment_hash.0);
            let claim_deadline_str = match claim_deadline {
              Some(deadline) => deadline.to_string(),
              None => "None".to_string(),
            };

            eprintln!(
              "[lightning-js] PaymentClaimable payment_hash={payment_hash_hex} claimable_amount_msat={claimable_amount_msat} claim_deadline={claim_deadline_str}",
            );
            pending_claims.insert(*payment_hash);
          }
          Event::PaymentReceived {
            payment_hash,
            amount_msat,
            ..
          } => {
            let payment_hash_hex = bytes_to_hex(&payment_hash.0);
            eprintln!(
              "[lightning-js] PaymentReceived payment_hash={payment_hash_hex} amount_msat={amount_msat}",
            );
            pending_claims.remove(payment_hash);
          }
          _ => {}
        }

        if let Event::PaymentReceived {
          payment_hash,
          amount_msat,
          ..
        } = event
        {
          received_payments.push(ReceivedPayment {
            payment_hash: payment_hash.to_string(),
            amount: amount_msat as i64,
          });
        }

        if let Err(err) = self.node.event_handled() {
          eprintln!("[lightning-js] Error while marking event handled: {err}");
        }
        last_event_time = now;
      }

      std::thread::sleep(POLL_INTERVAL);
    }

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node after receive_payment: {err}");
    }

    received_payments
  }

  #[napi]
  pub fn get_invoice(&self, amount: i64, description: String, expiry_secs: i64) -> PaymentMetadata {
    if let Err(err) = self.node.start() {
      eprintln!("[lightning-js] Failed to start node for get_invoice: {err}");
      panic!("failed to start node for get_invoice: {err}");
    }
    if let Err(err) = self.node.sync_wallets() {
      eprintln!("[lightning-js] Failed to sync wallets: {err}");
      panic!("failed to sync wallets: {err}");
    }

    let result = self.get_invoice_impl(Some(amount), description, expiry_secs);

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node after get_invoice: {err}");
    }

    result.unwrap()
  }

  /// Get invoice without starting/stopping the node.
  /// Use this when the node is already running via start_receiving().
  #[napi]
  pub fn get_invoice_while_running(
    &self,
    amount: i64,
    description: String,
    expiry_secs: i64,
  ) -> napi::Result<PaymentMetadata> {
    self.get_invoice_impl(Some(amount), description, expiry_secs)
  }

  /// Get variable amount invoice without starting/stopping the node.
  /// Use this when the node is already running via start_receiving().
  #[napi]
  pub fn get_variable_amount_jit_invoice_while_running(
    &self,
    description: String,
    expiry_secs: i64,
  ) -> napi::Result<PaymentMetadata> {
    self.get_invoice_impl(None, description, expiry_secs)
  }

  fn get_invoice_impl(
    &self,
    amount: Option<i64>,
    description: String,
    expiry_secs: i64,
  ) -> napi::Result<PaymentMetadata> {
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());

    let invoice = self
      .node
      .bolt11_payment()
      .receive_via_lsps4_jit_channel(
        amount.map(|a| a as u64),
        &bolt11_invoice_description,
        expiry_secs as u32,
      )
      .map_err(|e| napi::Error::from_reason(format!("Failed to get invoice: {e}")))?;

    Ok(invoice_to_payment_metadata(invoice))
  }

  #[napi]
  pub fn get_invoice_with_scid(
    &self,
    human_readable_scid: String,
    amount: i64,
    description: String,
    expiry_secs: i64,
  ) -> PaymentMetadata {
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());

    let scid = scid_from_human_readable_string(&human_readable_scid).unwrap();

    let bolt11 = self
      .node
      .bolt11_payment()
      .receive_via_lsps4_jit_channel_with_scid(
        scid,
        Some(amount as u64),
        &bolt11_invoice_description,
        expiry_secs as u32,
      )
      .unwrap();

    invoice_to_payment_metadata(bolt11)
  }

  #[napi]
  pub fn get_variable_amount_jit_invoice(
    &self,
    description: String,
    expiry_secs: i64,
  ) -> PaymentMetadata {
    // Note: this method doesn't start/stop the node (legacy behavior)
    self
      .get_invoice_impl(None, description, expiry_secs)
      .unwrap()
  }

  #[napi]
  pub fn get_variable_amount_jit_invoice_with_scid(
    &self,
    human_readable_scid: String,
    description: String,
    expiry_secs: i64,
  ) -> PaymentMetadata {
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());

    let scid = scid_from_human_readable_string(&human_readable_scid).unwrap();

    let bolt11 = self
      .node
      .bolt11_payment()
      .receive_via_lsps4_jit_channel_with_scid(
        scid,
        None,
        &bolt11_invoice_description,
        expiry_secs as u32,
      )
      .unwrap();

    invoice_to_payment_metadata(bolt11)
  }

  fn wait_for_payment_outcome(
    &self,
    payment_id: &PaymentId,
    timeout_secs: u64,
  ) -> napi::Result<()> {
    eprintln!(
      "[lightning-js] wait_for_payment_outcome start payment_id={} timeout_secs={}",
      bytes_to_hex(&payment_id.0),
      timeout_secs
    );

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    loop {
      if let Some(event) = self.node.next_event() {
        eprintln!(
          "[lightning-js] wait_for_payment_outcome saw event: {:?}",
          event
        );
        match event {
          Event::PaymentSuccessful {
            payment_id: event_payment_id,
            ..
          } => {
            self
              .node
              .event_handled()
              .map_err(|err| napi::Error::new(Status::GenericFailure, err.to_string()))?;

            if event_payment_id == Some(*payment_id) {
              eprintln!("[lightning-js] wait_for_payment_outcome success");
              return Ok(());
            }
          }
          Event::PaymentFailed {
            payment_id: Some(event_payment_id),
            reason,
            ..
          } => {
            self
              .node
              .event_handled()
              .map_err(|err| napi::Error::new(Status::GenericFailure, err.to_string()))?;

            if event_payment_id == *payment_id {
              let reason_str = reason
                .map(|r| format!("{r:?}"))
                .unwrap_or_else(|| "unknown reason".to_string());

              eprintln!("[lightning-js] wait_for_payment_outcome failure reason={reason_str}");
              return Err(napi::Error::new(
                Status::GenericFailure,
                format!("lnurl payment failed: {reason_str}"),
              ));
            }
          }
          _ => {
            eprintln!(
              "[lightning-js] wait_for_payment_outcome ignoring event: {:?}",
              event
            );
            self
              .node
              .event_handled()
              .map_err(|err| napi::Error::new(Status::GenericFailure, err.to_string()))?;
          }
        }
      }

      if Instant::now() >= deadline {
        eprintln!(
          "[lightning-js] Timed out waiting {timeout_secs}s for lnurl payment confirmation"
        );
        eprintln!("[lightning-js] wait_for_payment_outcome finished with timeout");
        return Ok(());
      }

      std::thread::sleep(POLL_INTERVAL);
    }
  }

  /// Unified payment method that auto-detects the destination type.
  ///
  /// Only supports variable-amount destinations where we set the amount:
  /// - BOLT12 offers (lno...)
  /// - LNURL (lnurl...)
  /// - Lightning addresses (user@domain)
  /// - Zero-amount BOLT11 invoices
  ///
  /// For fixed-amount BOLT11 invoices, amount_msat can be omitted (the invoice amount is used).
  /// For variable-amount destinations, amount_msat is required.
  #[napi]
  pub fn pay(
    &self,
    destination: String,
    amount_msat: Option<i64>,
    wait_for_payment_secs: Option<i64>,
  ) -> napi::Result<String> {
    eprintln!(
      "[lightning-js] pay called destination={} amount_msat={:?} wait_for_payment_secs={:?}",
      destination, amount_msat, wait_for_payment_secs
    );

    let (payment_target, wait_secs) =
      self.resolve_payment_target(destination, amount_msat, wait_for_payment_secs)?;

    // Start node
    self.node.start().map_err(|e| {
      napi::Error::new(Status::GenericFailure, format!("failed to start node: {e}"))
    })?;

    // Sync wallets
    if let Err(e) = self.node.sync_wallets() {
      let _ = self.node.stop();
      return Err(napi::Error::new(
        Status::GenericFailure,
        format!("failed to sync wallets: {e}"),
      ));
    }

    let result = self.execute_payment_impl(&payment_target, wait_secs);
    let _ = self.node.stop();
    result
  }

  /// Unified payment method that auto-detects the destination type.
  /// Use this when the node is already running via start_receiving().
  ///
  /// Supports all destination types:
  /// - BOLT11 invoices (fixed or variable amount)
  /// - BOLT12 offers (lno...)
  /// - LNURL (lnurl...)
  /// - Lightning addresses (user@domain)
  ///
  /// For fixed-amount BOLT11 invoices, amount_msat can be omitted (the invoice amount is used).
  /// For variable-amount destinations, amount_msat is required.
  #[napi]
  pub fn pay_while_running(
    &self,
    destination: String,
    amount_msat: Option<i64>,
    wait_for_payment_secs: Option<i64>,
  ) -> napi::Result<String> {
    eprintln!(
      "[lightning-js] pay_while_running called destination={} amount_msat={:?} wait_for_payment_secs={:?}",
      destination, amount_msat, wait_for_payment_secs
    );

    let (payment_target, wait_secs) =
      self.resolve_payment_target(destination, amount_msat, wait_for_payment_secs)?;

    self.execute_payment_impl(&payment_target, wait_secs)
  }

  /// Parse destination and resolve to payment target
  fn resolve_payment_target(
    &self,
    destination: String,
    amount_msat: Option<i64>,
    wait_for_payment_secs: Option<i64>,
  ) -> napi::Result<(PaymentTarget, Option<u64>)> {
    let wait_secs = wait_for_payment_secs.and_then(|s| if s > 0 { Some(s as u64) } else { None });

    // Parse destination and resolve to payment method
    let resolver = HTTPHrnResolver::new();
    let runtime = create_current_thread_runtime()?;

    eprintln!("[lightning-js] parsing destination");
    let payment_instructions = runtime
      .block_on(PaymentInstructions::parse(
        &destination,
        self.network,
        &resolver,
        true,
      ))
      .map_err(|err| {
        napi::Error::new(
          Status::InvalidArg,
          format!("failed to parse destination: {err:?}"),
        )
      })?;

    // Get payment methods and amount based on instruction type
    let (methods, final_amount_msat): (&[PaymentMethod], u64) = match &payment_instructions {
      PaymentInstructions::FixedAmount(fixed) => {
        // Use the amount from the invoice
        let invoice_amount = fixed
          .ln_payment_amount()
          .ok_or_else(|| {
            napi::Error::new(
              Status::InvalidArg,
              "fixed-amount destination has no lightning amount",
            )
          })?
          .milli_sats();
        eprintln!(
          "[lightning-js] fixed-amount destination: {}msat",
          invoice_amount
        );
        (fixed.methods(), invoice_amount)
      }
      PaymentInstructions::ConfigurableAmount(configurable) => {
        // Amount is required for configurable-amount destinations
        let amount_msat = amount_msat.ok_or_else(|| {
          napi::Error::new(
            Status::InvalidArg,
            "amount_msat is required for variable-amount destinations",
          )
        })?;
        let amount_msat = u64::try_from(amount_msat)
          .ok()
          .filter(|&a| a > 0)
          .ok_or_else(|| {
            napi::Error::new(Status::InvalidArg, "amount_msat must be greater than zero")
          })?;

        // Clone and call helper to handle ownership
        return self.resolve_configurable_payment(
          configurable.clone(),
          amount_msat,
          wait_secs,
          &resolver,
          &runtime,
        );
      }
    };

    eprintln!(
      "[lightning-js] resolved {} payment method(s)",
      methods.len()
    );

    let payment_target = methods
      .iter()
      .find_map(|m| match m {
        PaymentMethod::LightningBolt11(inv) => Bolt11Invoice::from_str(&inv.to_string())
          .ok()
          .map(|i| PaymentTarget::Bolt11(i, final_amount_msat)),
        PaymentMethod::LightningBolt12(offer) => Offer::from_str(&offer.to_string())
          .ok()
          .map(|o| PaymentTarget::Bolt12(Box::new(o), final_amount_msat)),
        _ => None,
      })
      .ok_or_else(|| {
        napi::Error::new(Status::GenericFailure, "no supported payment method found")
      })?;

    Ok((payment_target, wait_secs))
  }

  /// Helper for configurable amount payments (handles lifetime issues)
  fn resolve_configurable_payment(
    &self,
    configurable: bitcoin_payment_instructions::ConfigurableAmountPaymentInstructions,
    amount_msat: u64,
    wait_secs: Option<u64>,
    resolver: &HTTPHrnResolver,
    runtime: &Runtime,
  ) -> napi::Result<(PaymentTarget, Option<u64>)> {
    let requested_amount = InstructionAmount::from_milli_sats(amount_msat)
      .map_err(|_| napi::Error::new(Status::InvalidArg, "amount exceeds maximum"))?;
    let fixed = runtime
      .block_on(configurable.set_amount(requested_amount, resolver))
      .map_err(|err| {
        napi::Error::new(
          Status::GenericFailure,
          format!("failed to set amount: {err}"),
        )
      })?;

    let methods = fixed.methods();
    eprintln!(
      "[lightning-js] resolved {} payment method(s)",
      methods.len()
    );

    let payment_target = methods
      .iter()
      .find_map(|m| match m {
        PaymentMethod::LightningBolt11(inv) => Bolt11Invoice::from_str(&inv.to_string())
          .ok()
          .map(|i| PaymentTarget::Bolt11(i, amount_msat)),
        PaymentMethod::LightningBolt12(offer) => Offer::from_str(&offer.to_string())
          .ok()
          .map(|o| PaymentTarget::Bolt12(Box::new(o), amount_msat)),
        _ => None,
      })
      .ok_or_else(|| {
        napi::Error::new(Status::GenericFailure, "no supported payment method found")
      })?;

    // Validate BOLT11 invoice amount matches requested (protects against malicious LNURL services)
    if let PaymentTarget::Bolt11(ref invoice, _) = payment_target {
      if let Some(invoice_amount) = invoice.amount_milli_satoshis() {
        if invoice_amount != amount_msat {
          return Err(napi::Error::new(
            Status::InvalidArg,
            format!(
              "invoice amount ({invoice_amount}msat) does not match requested amount ({amount_msat}msat)"
            ),
          ));
        }
      }
    }

    Ok((payment_target, wait_secs))
  }

  /// Core payment execution logic (no start/stop)
  fn execute_payment_impl(
    &self,
    target: &PaymentTarget,
    wait_secs: Option<u64>,
  ) -> napi::Result<String> {
    // BOLT12 requires full RGS sync for onion message routing
    if matches!(target, PaymentTarget::Bolt12(_, _)) {
      eprintln!("[lightning-js] doing full RGS sync for BOLT12");
      let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
      rt.block_on(async {
        match self.node.sync_rgs(true).await {
          Ok(ts) => eprintln!("[lightning-js] RGS sync complete, timestamp={ts}"),
          Err(e) => eprintln!("[lightning-js] RGS sync failed: {e}"),
        }
      });
    }

    // Wait for channels and check balance
    wait_for_usable_channels(&self.node);
    let available = usable_outbound_capacity_msat(&self.node);
    eprintln!("[lightning-js] available outbound capacity: {available}msat");

    let required = target.amount_msat();
    if available < required {
      return Err(napi::Error::new(
        Status::GenericFailure,
        format!(
          "insufficient outbound capacity: required {required}msat, available {available}msat"
        ),
      ));
    }

    // Send payment
    let payment_id = match target {
      PaymentTarget::Bolt11(invoice, amount) => {
        eprintln!(
          "[lightning-js] sending BOLT11 payment_hash={} amount={}msat",
          invoice.payment_hash(),
          amount
        );
        self
          .node
          .bolt11_payment()
          .send_using_amount(invoice, *amount, None)
      }
      PaymentTarget::Bolt12(offer, amount) => {
        eprintln!("[lightning-js] sending BOLT12 offer_id={}", offer.id());
        self.node.bolt12_payment().send_using_amount(
          offer,
          *amount,
          None,
          Some("A payment by MoneyDevKit".to_string()),
          None,
        )
      }
    }
    .map_err(|e| {
      napi::Error::new(
        Status::GenericFailure,
        format!("failed to send payment: {e}"),
      )
    })?;

    eprintln!(
      "[lightning-js] payment sent, id={}",
      bytes_to_hex(&payment_id.0)
    );

    // Wait for outcome if requested
    if let Some(secs) = wait_secs {
      self.wait_for_payment_outcome(&payment_id, secs)?;
    }

    Ok(bytes_to_hex(&payment_id.0))
  }
}

/// Wait for all channels to become usable after node startup/sync.
fn wait_for_usable_channels(node: &Node) {
  let start = Instant::now();

  loop {
    let channels = node.list_channels();
    let total = channels.len();
    let usable = channels.iter().filter(|c| c.is_usable).count();

    if total > 0 && usable == total {
      eprintln!(
        "[lightning-js] All channels usable ({usable}/{total}) after {}ms",
        start.elapsed().as_millis()
      );
      return;
    }

    if start.elapsed() >= CHANNEL_USABLE_TIMEOUT {
      eprintln!(
        "[lightning-js] Timeout: {usable}/{total} channels usable after {}s",
        CHANNEL_USABLE_TIMEOUT.as_secs()
      );
      return;
    }

    std::thread::sleep(POLL_INTERVAL);
  }
}

/// Compute total outbound capacity across all usable channels.
fn usable_outbound_capacity_msat(node: &Node) -> u64 {
  node
    .list_channels()
    .iter()
    .filter(|c| c.is_usable)
    .map(|c| c.outbound_capacity_msat)
    .sum()
}

fn scid_from_human_readable_string(human_readable_scid: &str) -> Result<u64, ()> {
  let mut parts = human_readable_scid.split('x');

  let block: u64 = parts.next().ok_or(())?.parse().map_err(|_e| ())?;
  let tx_index: u64 = parts.next().ok_or(())?.parse().map_err(|_e| ())?;
  let vout_index: u64 = parts.next().ok_or(())?.parse().map_err(|_e| ())?;

  Ok((block << 40) | (tx_index << 16) | vout_index)
}

fn invoice_to_payment_metadata(invoice: Bolt11Invoice) -> PaymentMetadata {
  let route_hints = invoice.route_hints();
  let first_route_hint = route_hints.first().unwrap();
  let hint_hop = first_route_hint.0.first().unwrap();
  let scid = hint_hop.short_channel_id;

  let human_readable_scid = scid_to_string(scid);

  PaymentMetadata {
    bolt11: invoice.to_string(),
    payment_hash: invoice.payment_hash().to_string(),
    expires_at: invoice.expires_at().unwrap().as_secs() as i64,
    scid: human_readable_scid,
  }
}

fn scid_to_string(scid: u64) -> String {
  let block = scid_utils::block_from_scid(scid);
  let tx_index = scid_utils::tx_index_from_scid(scid);
  let vout = scid_utils::vout_from_scid(scid);
  format!("{block}x{tx_index}x{vout}")
}

fn bytes_to_hex(bytes: &[u8]) -> String {
  let mut hex = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    let _ = write!(&mut hex, "{:02x}", byte);
  }
  hex
}

fn u64_to_i64(value: u64) -> i64 {
  i64::try_from(value).unwrap_or(i64::MAX)
}

fn create_current_thread_runtime() -> Result<Runtime, napi::Error> {
  tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| {
      napi::Error::new(
        Status::GenericFailure,
        format!("failed to initialize async runtime: {error}"),
      )
    })
}
