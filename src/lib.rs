#![deny(clippy::all)]

use std::{collections::HashMap, str::FromStr};

use ldk_node::{
  bip39::Mnemonic,
  bitcoin::{secp256k1::PublicKey, Network},
  generate_entropy_mnemonic,
  lightning::ln::msgs::SocketAddress,
  lightning::util::scid_utils,
  lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description},
  Builder, Event, Node,
};

#[macro_use]
extern crate napi_derive;

#[napi]
pub fn generate_mnemonic() -> String {
  generate_entropy_mnemonic().to_string()
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

#[napi]
pub struct MdkNode {
  node: Node,
}

#[napi]
impl MdkNode {
  #[napi(constructor)]
  pub fn new(options: MdkNodeOptions) -> Self {
    let network = match options.network.as_str() {
      "mainnet" => Network::Bitcoin,
      "testnet" => Network::Testnet,
      "signet" => Network::Signet,
      "regtest" => Network::Regtest,
      _ => Network::Signet,
    };

    let mnemonic = Mnemonic::from_str(&options.mnemonic).unwrap();
    let lsp_node_id = PublicKey::from_str(&options.lsp_node_id).unwrap();
    let lsp_address = SocketAddress::from_str(&options.lsp_address).unwrap();

    let mut builder = Builder::new();
    builder.set_network(network);
    builder.set_chain_source_esplora(options.esplora_url, None);
    builder.set_gossip_source_rgs(options.rgs_url);
    builder.set_entropy_bip39_mnemonic(mnemonic, None);
    builder.set_log_facade_logger();
    builder.set_liquidity_source_lsps4(lsp_node_id, lsp_address);

    let vss_headers = HashMap::from([(
      "Authorization".to_string(),
      format!("Bearer {}", options.mdk_api_key),
    )]);

    // TODO: probably want to replace store_id with something generated from mnemonic?
    let node = builder
      .build_with_vss_store_and_fixed_headers(options.vss_url, options.mdk_api_key, vss_headers)
      .unwrap();

    Self { node }
  }

  #[napi]
  pub fn get_node_id(&self) -> String {
    self.node.node_id().to_string()
  }

  #[napi]
  pub fn start(&self) {
    self.node.start().unwrap();
  }

  #[napi]
  pub fn stop(&self) {
    self.node.stop().unwrap();
  }

  #[napi]
  pub fn sync_wallets(&self) {
    self.node.sync_wallets().unwrap();
  }

  #[napi]
  pub fn receive_payment(
    &self,
    min_threshold_ms: i64,
    quiet_threshold_ms: i64,
  ) -> Vec<ReceivedPayment> {
    let mut received_payments = vec![];

    if self.node.start().is_err() {
      return received_payments;
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

        let _ = self.node.event_handled();
        last_event_time = now;
      }

      std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let _ = self.node.stop();

    received_payments
  }

  #[napi]
  pub fn get_invoice(&self, amount: i64, description: String, expiry_secs: i64) -> PaymentMetadata {
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());
    self.node.start().unwrap();

    let invoice = self
      .node
      .bolt11_payment()
      .receive_via_lsps4_jit_channel(
        Some(amount as u64),
        &bolt11_invoice_description,
        expiry_secs as u32,
      )
      .unwrap();

    let _ = self.node.stop();

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

  let block = scid_utils::block_from_scid(scid);
  let tx_index = scid_utils::tx_index_from_scid(scid);
  let vout = scid_utils::vout_from_scid(scid);

  let human_readable_scid = format!("{}x{}x{}", block, tx_index, vout);

  PaymentMetadata {
    bolt11: invoice.to_string(),
    payment_hash: invoice.payment_hash().to_string(),
    expires_at: invoice.expires_at().unwrap().as_secs() as i64,
    scid: human_readable_scid,
  }
}
