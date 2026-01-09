#![deny(clippy::all)]

use std::{
  collections::HashMap,
  convert::TryFrom,
  fmt::{self, Write},
  str::FromStr,
  sync::{
    atomic::{AtomicU8, Ordering},
    Arc, OnceLock, RwLock,
  },
  time::{Duration, Instant},
};

use bitcoin_payment_instructions::{
  amount::Amount as InstructionAmount, hrn_resolution::HrnResolver, http_resolver::HTTPHrnResolver,
  ParseError, PaymentInstructions, PaymentMethod,
};
use napi::{
  threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
  Env, JsFunction, Status,
};

use ldk_node::logger::{LogLevel, LogRecord, LogWriter};
use ldk_node::{
  bip39::Mnemonic,
  bitcoin::{
    hashes::{sha256, Hash},
    secp256k1::PublicKey,
    Network,
  },
  generate_entropy_mnemonic,
  lightning::ln::channelmanager::PaymentId,
  lightning::{
    ln::msgs::SocketAddress,
    offers::offer::{Amount, Offer},
    util::scid_utils,
  },
  lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description},
  Builder, Event, Node,
};
use tokio::runtime::Runtime;

#[macro_use]
extern crate napi_derive;

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
  generate_entropy_mnemonic().to_string()
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

    let mut builder = Builder::new();
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

    let total_outbound_msat = self
      .node
      .list_channels()
      .into_iter()
      .fold(0u64, |acc, channel| {
        acc.saturating_add(channel.outbound_capacity_msat)
      });

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node via stop() in get_balance: {err}");
      panic!("failed to stop node: {err}");
    }

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
    let mut received_payments = vec![];

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

      if total_time_elapsed >= min_threshold_ms && quiet_time_elapsed >= quiet_threshold_ms {
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

      std::thread::sleep(std::time::Duration::from_millis(10));
    }

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node after receive_payment: {err}");
    }

    received_payments
  }

  #[napi]
  pub fn get_invoice(&self, amount: i64, description: String, expiry_secs: i64) -> PaymentMetadata {
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());
    if let Err(err) = self.node.start() {
      eprintln!("[lightning-js] Failed to start node for get_invoice: {err}");
      panic!("failed to start node for get_invoice: {err}");
    }
    if let Err(err) = self.node.sync_wallets() {
      eprintln!("[lightning-js] Failed to sync wallets: {err}");
      panic!("failed to sync wallets: {err}");
    }

    let invoice = self
      .node
      .bolt11_payment()
      .receive_via_lsps4_jit_channel(
        Some(amount as u64),
        &bolt11_invoice_description,
        expiry_secs as u32,
      )
      .unwrap();

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node after get_invoice: {err}");
    }

    invoice_to_payment_metadata(invoice)
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
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());
    let bolt11 = self
      .node
      .bolt11_payment()
      .receive_via_lsps4_jit_channel(None, &bolt11_invoice_description, expiry_secs as u32)
      .unwrap();

    invoice_to_payment_metadata(bolt11)
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

      std::thread::sleep(Duration::from_millis(50));
    }
  }

  #[napi]
  pub fn pay_lnurl(
    &self,
    lnurl: String,
    amount_msat: i64,
    wait_for_payment_secs: Option<i64>,
  ) -> napi::Result<String> {
    eprintln!(
      "[lightning-js] pay_lnurl called lnurl={} amount_msat={} wait_for_payment_secs={:?}",
      lnurl, amount_msat, wait_for_payment_secs
    );

    if amount_msat <= 0 {
      return Err(napi::Error::new(
        Status::InvalidArg,
        "amount must be greater than zero".to_string(),
      ));
    }

    let amount_msat_u64 = u64::try_from(amount_msat).map_err(|_| {
      napi::Error::new(
        Status::InvalidArg,
        "amount must be representable as an unsigned 64-bit value".to_string(),
      )
    })?;

    let requested_amount = InstructionAmount::from_milli_sats(amount_msat_u64).map_err(|_| {
      napi::Error::new(
        Status::InvalidArg,
        "amount exceeds supported maximum (21 million BTC)".to_string(),
      )
    })?;

    let wait_for_payment_secs = wait_for_payment_secs.unwrap_or(0);
    let wait_for_payment_secs = if wait_for_payment_secs > 0 {
      Some(wait_for_payment_secs as u64)
    } else {
      None
    };
    eprintln!(
      "[lightning-js] pay_lnurl wait_for_payment_secs_normalized={wait_for_payment_secs:?}"
    );

    let resolver = HTTPHrnResolver::new();
    let runtime = create_current_thread_runtime().map_err(lnurl_error_to_napi)?;
    eprintln!("[lightning-js] pay_lnurl resolving lnurl invoice");
    let invoice = resolve_lnurl_invoice_with_runtime(
      &runtime,
      &resolver,
      &lnurl,
      requested_amount,
      self.network,
    )
    .map_err(lnurl_error_to_napi)?;
    eprintln!(
      "[lightning-js] pay_lnurl resolved invoice payment_hash={}",
      invoice.payment_hash()
    );

    eprintln!("[lightning-js] pay_lnurl starting node");
    self.node.start().map_err(|error| {
      napi::Error::new(
        Status::GenericFailure,
        format!("failed to start node prior to paying lnurl: {error}"),
      )
    })?;

    eprintln!("[lightning-js] pay_lnurl syncing wallets");
    if let Err(err) = self.node.sync_wallets() {
      eprintln!("[lightning-js] Failed to sync wallets: {err}");
      panic!("failed to sync wallets: {err}");
    }

    let available_balance_msat: u64 = self
      .node
      .list_channels()
      .into_iter()
      .filter(|channel| channel.is_channel_ready)
      .map(|channel| channel.outbound_capacity_msat)
      .sum();
    eprintln!("[lightning-js] pay_lnurl available_balance_msat={available_balance_msat}");

    if available_balance_msat == 0 {
      if let Err(err) = self.node.stop() {
        eprintln!(
          "[lightning-js] Failed to stop node after checking lnurl outbound capacity: {err}"
        );
      }
      return Err(napi::Error::new(
        Status::GenericFailure,
        "unable to pay lnurl without outbound capacity".to_string(),
      ));
    }

    if available_balance_msat < amount_msat_u64 {
      if let Err(err) = self.node.stop() {
        eprintln!(
          "[lightning-js] Failed to stop node after insufficient lnurl outbound capacity: {err}"
        );
      }
      return Err(napi::Error::new(
        Status::GenericFailure,
        format!(
          "insufficient outbound capacity to pay lnurl: required {}msat, available {}msat",
          amount_msat_u64, available_balance_msat,
        ),
      ));
    }

    let payment_id = match self.node.bolt11_payment().send(&invoice, None) {
      Ok(payment_id) => payment_id,
      Err(error) => {
        eprintln!("[lightning-js] pay_lnurl send error: {error}");
        if let Err(stop_error) = self.node.stop() {
          eprintln!("[lightning-js] Failed to stop node after lnurl send error: {stop_error}");
        }
        return Err(napi::Error::new(
          Status::GenericFailure,
          format!("failed to send lnurl payment: {error}"),
        ));
      }
    };
    eprintln!(
      "[lightning-js] pay_lnurl send ok payment_id={}",
      bytes_to_hex(&payment_id.0)
    );

    if let Some(wait_secs) = wait_for_payment_secs {
      eprintln!("[lightning-js] pay_lnurl waiting for payment outcome wait_secs={wait_secs}");
      let wait_result = self.wait_for_payment_outcome(&payment_id, wait_secs);

      if let Err(err) = self.node.stop() {
        eprintln!("[lightning-js] Failed to stop node after lnurl payment wait: {err}");
      }

      wait_result?;
    } else if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node after successful lnurl payment: {err}");
    }

    eprintln!(
      "[lightning-js] pay_lnurl returning payment_id={}",
      bytes_to_hex(&payment_id.0)
    );
    Ok(bytes_to_hex(&payment_id.0))
  }

  #[napi]
  pub fn pay_bolt_11(&self, bolt11_invoice: String) -> napi::Result<String> {
    let invoice = Bolt11Invoice::from_str(&bolt11_invoice).map_err(|error| {
      napi::Error::new(
        Status::InvalidArg,
        format!("failed to parse bolt11 invoice: {error}"),
      )
    })?;

    let amount_msat = invoice.amount_milli_satoshis().ok_or_else(|| {
      napi::Error::new(
        Status::InvalidArg,
        "bolt11 invoice is missing an amount and cannot be paid".to_string(),
      )
    })?;

    self.node.start().map_err(|error| {
      napi::Error::new(
        Status::GenericFailure,
        format!("failed to start node prior to paying bolt11 invoice: {error}"),
      )
    })?;

    let available_balance_msat: u64 = self
      .node
      .list_channels()
      .into_iter()
      .filter(|channel| channel.is_channel_ready)
      .map(|channel| channel.outbound_capacity_msat)
      .sum();

    if available_balance_msat == 0 {
      if let Err(err) = self.node.stop() {
        eprintln!(
          "[lightning-js] Failed to stop node after checking bolt11 outbound capacity: {err}"
        );
      }
      return Err(napi::Error::new(
        Status::GenericFailure,
        "unable to pay bolt11 invoice without outbound capacity".to_string(),
      ));
    }

    if available_balance_msat < amount_msat {
      if let Err(err) = self.node.stop() {
        eprintln!(
          "[lightning-js] Failed to stop node after insufficient bolt11 outbound capacity: {err}"
        );
      }
      return Err(napi::Error::new(
        Status::GenericFailure,
        format!(
          "insufficient outbound capacity to pay bolt11 invoice: required {}msat, available {}msat",
          amount_msat, available_balance_msat,
        ),
      ));
    }

    let payment_id = match self.node.bolt11_payment().send(&invoice, None) {
      Ok(payment_id) => payment_id,
      Err(error) => {
        if let Err(stop_error) = self.node.stop() {
          eprintln!(
            "[lightning-js] Failed to stop node after bolt11 payment send error: {stop_error}"
          );
        }
        return Err(napi::Error::new(
          Status::GenericFailure,
          format!("failed to send bolt11 payment: {error}"),
        ));
      }
    };

    if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node after successful bolt11 payment: {err}");
    }

    Ok(bytes_to_hex(&payment_id.0))
  }

  #[napi]
  pub fn pay_bolt12_offer(
    &self,
    bolt12_offer_string: String,
    amount_msat: i64,
    wait_for_payment_secs: Option<i64>,
  ) -> napi::Result<String> {
    eprintln!(
      "[lightning-js] pay_bolt12_offer called amount_msat={} wait_for_payment_secs={:?}",
      amount_msat, wait_for_payment_secs
    );
    eprintln!(
      "[lightning-js] pay_bolt12_offer bolt12_offer={}",
      bolt12_offer_string
    );

    if amount_msat <= 0 {
      return Err(napi::Error::new(
        Status::InvalidArg,
        "amount must be greater than zero".to_string(),
      ));
    }

    let amount_msat_u64 = u64::try_from(amount_msat).map_err(|_| {
      napi::Error::new(
        Status::InvalidArg,
        "amount must be representable as an unsigned 64-bit value".to_string(),
      )
    })?;

    let wait_for_payment_secs = wait_for_payment_secs.unwrap_or(0);
    let wait_for_payment_secs = if wait_for_payment_secs > 0 {
      Some(wait_for_payment_secs as u64)
    } else {
      None
    };
    eprintln!(
      "[lightning-js] pay_bolt12_offer wait_for_payment_secs_normalized={wait_for_payment_secs:?}"
    );

    let bolt12_offer = Offer::from_str(&bolt12_offer_string)
      .map_err(|_| napi::Error::new(Status::InvalidArg, "invalid bolt12 offer".to_string()))?;

    eprintln!(
      "[lightning-js] pay_bolt12_offer parsed offer: id={} issuer_pubkey={:?} description={:?} amount={:?}",
      bolt12_offer.id(),
      bolt12_offer.issuer_signing_pubkey(),
      bolt12_offer.description(),
      bolt12_offer.amount()
    );

    eprintln!("[lightning-js] pay_bolt12_offer starting node");
    self.node.start().map_err(|error| {
      napi::Error::new(
        Status::GenericFailure,
        format!("failed to start node prior to paying offer: {error}"),
      )
    })?;

    // Full RGS sync to get node announcements (addresses/features) needed for BOLT12
    eprintln!("[lightning-js] pay_bolt12_offer doing full RGS sync");
    let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime for RGS sync");
    rt.block_on(async {
      match self.node.sync_rgs(true).await {
        Ok(ts) => eprintln!(
          "[lightning-js] pay_bolt12_offer RGS sync complete, timestamp={}",
          ts
        ),
        Err(e) => eprintln!("[lightning-js] pay_bolt12_offer RGS sync failed: {}", e),
      }
    });

    eprintln!("[lightning-js] pay_bolt12_offer syncing wallets");
    if let Err(err) = self.node.sync_wallets() {
      eprintln!("[lightning-js] Failed to sync wallets: {err}");
      panic!("failed to sync wallets: {err}");
    }
    eprintln!("[lightning-js] pay_bolt12_offer wallet sync complete");

    let channels = self.node.list_channels();
    let ready_channels: Vec<_> = channels
      .iter()
      .filter(|channel| channel.is_channel_ready)
      .collect();
    let available_balance_msat: u64 = ready_channels
      .iter()
      .map(|channel| channel.outbound_capacity_msat)
      .sum();

    eprintln!(
      "[lightning-js] pay_bolt12_offer channels: total={} ready={} available_balance_msat={}",
      channels.len(),
      ready_channels.len(),
      available_balance_msat
    );

    if available_balance_msat == 0 {
      if let Err(err) = self.node.stop() {
        eprintln!(
          "[lightning-js] Failed to stop node after checking bolt12 outbound capacity: {err}"
        );
      }
      return Err(napi::Error::new(
        Status::GenericFailure,
        "unable to pay bolt12 offer without outbound capacity".to_string(),
      ));
    }

    let amount_to_send_msat = match bolt12_offer.amount() {
      Some(Amount::Bitcoin { amount_msats }) => amount_msats,
      Some(_) => {
        if let Err(err) = self.node.stop() {
          eprintln!("[lightning-js] Failed to stop node after unsupported bolt12 currency: {err}");
        }
        return Err(napi::Error::new(
          Status::GenericFailure,
          "unsupported currency in bolt12 offer".to_string(),
        ));
      }
      None => amount_msat_u64,
    };

    eprintln!(
      "[lightning-js] pay_bolt12_offer amount_to_send_msat={}",
      amount_to_send_msat
    );

    if amount_to_send_msat == 0 {
      if let Err(err) = self.node.stop() {
        eprintln!("[lightning-js] Failed to stop node after zero-amount bolt12 offer: {err}");
      }
      return Err(napi::Error::new(
        Status::GenericFailure,
        "bolt12 offer amount resolves to zero".to_string(),
      ));
    }

    if available_balance_msat < amount_to_send_msat {
      if let Err(err) = self.node.stop() {
        eprintln!("[lightning-js] Failed to stop node after insufficient outbound capacity: {err}");
      }
      return Err(napi::Error::new(
        Status::GenericFailure,
        format!(
          "insufficient outbound capacity to pay offer: required {}msat, available {}msat",
          amount_to_send_msat, available_balance_msat,
        ),
      ));
    }

    eprintln!(
      "[lightning-js] pay_bolt12_offer sending payment: offer_id={} amount_msat={}",
      bolt12_offer.id(),
      amount_to_send_msat
    );

    let payment_id = match self.node.bolt12_payment().send_using_amount(
      &bolt12_offer,
      amount_to_send_msat,
      None,
      Some("A payment by MoneyDevKit".to_string()),
    ) {
      Ok(payment_id) => payment_id,
      Err(error) => {
        eprintln!("[lightning-js] pay_bolt12_offer send error: {error}");
        if let Err(stop_error) = self.node.stop() {
          eprintln!("[lightning-js] Failed to stop node after bolt12 send error: {stop_error}");
        }
        return Err(napi::Error::new(
          Status::GenericFailure,
          format!("failed to send bolt12 offer payment: {error}"),
        ));
      }
    };
    eprintln!(
      "[lightning-js] pay_bolt12_offer send ok payment_id={}",
      bytes_to_hex(&payment_id.0)
    );

    if let Some(wait_secs) = wait_for_payment_secs {
      eprintln!(
        "[lightning-js] pay_bolt12_offer waiting for payment outcome wait_secs={wait_secs}"
      );
      let wait_result = self.wait_for_payment_outcome(&payment_id, wait_secs);

      if let Err(err) = self.node.stop() {
        eprintln!("[lightning-js] Failed to stop node after bolt12 payment wait: {err}");
      }

      wait_result?;
    } else if let Err(err) = self.node.stop() {
      eprintln!("[lightning-js] Failed to stop node after successful bolt12 payment: {err}");
    }

    eprintln!(
      "[lightning-js] pay_bolt12_offer returning payment_id={}",
      bytes_to_hex(&payment_id.0)
    );
    Ok(bytes_to_hex(&payment_id.0))
  }
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

#[derive(Debug)]
enum LnurlPayError {
  RuntimeInit(String),
  Parse(ParseError),
  Finalization(&'static str),
  MissingInvoice,
  InvoiceParse(String),
  AmountMismatch {
    invoice_msat: u64,
    requested_msat: u64,
  },
}

impl fmt::Display for LnurlPayError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::RuntimeInit(err) => write!(f, "failed to initialize async runtime for lnurl: {err}"),
      Self::Parse(err) => write!(f, "failed to parse lnurl instructions: {err:?}"),
      Self::Finalization(err) => write!(f, "failed to finalize lnurl amount: {err}"),
      Self::MissingInvoice => write!(
        f,
        "payment instructions did not resolve to a lightning invoice"
      ),
      Self::InvoiceParse(err) => write!(f, "failed to parse resolved lightning invoice: {err}"),
      Self::AmountMismatch {
        invoice_msat,
        requested_msat,
      } => write!(
        f,
        "invoice amount ({invoice_msat}msat) does not match requested amount ({requested_msat}msat)"
      ),
    }
  }
}

impl std::error::Error for LnurlPayError {}

fn lnurl_error_to_napi(err: LnurlPayError) -> napi::Error {
  let status = match err {
    LnurlPayError::Parse(_) | LnurlPayError::Finalization(_) => Status::InvalidArg,
    _ => Status::GenericFailure,
  };
  napi::Error::new(status, err.to_string())
}

fn create_current_thread_runtime() -> Result<Runtime, LnurlPayError> {
  tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| LnurlPayError::RuntimeInit(error.to_string()))
}

fn resolve_lnurl_invoice_with_runtime<R: HrnResolver>(
  runtime: &Runtime,
  resolver: &R,
  lnurl: &str,
  requested_amount: InstructionAmount,
  network: Network,
) -> Result<Bolt11Invoice, LnurlPayError> {
  let payment_instructions = runtime
    .block_on(PaymentInstructions::parse(lnurl, network, resolver, true))
    .map_err(LnurlPayError::Parse)?;

  let fixed_instructions = match payment_instructions {
    PaymentInstructions::FixedAmount(fixed) => fixed,
    PaymentInstructions::ConfigurableAmount(configurable) => runtime
      .block_on(configurable.set_amount(requested_amount, resolver))
      .map_err(LnurlPayError::Finalization)?,
  };

  let invoice_string = fixed_instructions
    .methods()
    .iter()
    .find_map(|method| match method {
      PaymentMethod::LightningBolt11(invoice) => Some(invoice.to_string()),
      _ => None,
    })
    .ok_or(LnurlPayError::MissingInvoice)?;

  let invoice = Bolt11Invoice::from_str(&invoice_string)
    .map_err(|error| LnurlPayError::InvoiceParse(error.to_string()))?;

  let invoice_amount_msat = invoice
    .amount_milli_satoshis()
    .ok_or(LnurlPayError::MissingInvoice)?;
  let requested_msat = requested_amount.milli_sats();

  if invoice_amount_msat != requested_msat {
    return Err(LnurlPayError::AmountMismatch {
      invoice_msat: invoice_amount_msat,
      requested_msat,
    });
  }

  Ok(invoice)
}
