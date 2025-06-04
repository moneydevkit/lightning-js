#![deny(clippy::all)]

use std::str::FromStr;

use ldk_node::{
  bip39::Mnemonic,
  bitcoin::{secp256k1::PublicKey, Network},
  generate_entropy_mnemonic,
  lightning::ln::msgs::SocketAddress,
  lightning_invoice::{Bolt11InvoiceDescription, Description},
  Builder, Node,
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
  pub esplora_url: String,
  pub rgs_url: String,
  pub mnemonic: String,
  pub lsp_node_id: String,
  pub lsp_address: String,
  pub lsp_token: Option<String>,
}

#[napi(object)]
pub struct PaymentMetadata {
  pub bolt11: String,
  pub payment_hash: String,
  pub expires_at: i64,
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
    builder.set_liquidity_source_lsps2(lsp_node_id, lsp_address, options.lsp_token);

    let node = builder.build().unwrap();

    Self { node }
  }

  #[napi]
  pub fn get_invoice(&self, amount: i64, description: String, expiry_secs: i64) -> PaymentMetadata {
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());
    let bolt11 = self
      .node
      .bolt11_payment()
      .receive(
        amount as u64,
        &bolt11_invoice_description,
        expiry_secs as u32,
      )
      .unwrap();

    PaymentMetadata {
      bolt11: bolt11.to_string(),
      payment_hash: bolt11.payment_hash().to_string(),
      expires_at: bolt11.expires_at().unwrap().as_secs() as i64,
    }
  }

  #[napi]
  pub fn get_variable_amount_invoice(
    &self,
    description: String,
    expiry_secs: i64,
  ) -> PaymentMetadata {
    let bolt11_invoice_description =
      Bolt11InvoiceDescription::Direct(Description::new(description).unwrap());
    let bolt11 = self
      .node
      .bolt11_payment()
      .receive_variable_amount(&bolt11_invoice_description, expiry_secs as u32)
      .unwrap();

    PaymentMetadata {
      bolt11: bolt11.to_string(),
      payment_hash: bolt11.payment_hash().to_string(),
      expires_at: bolt11.expires_at().unwrap().as_secs() as i64,
    }
  }
}
