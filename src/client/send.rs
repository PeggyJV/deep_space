use crate::address::Address;
use crate::client::Contact;
use crate::client::MEMO;
use crate::coin::Coin;
use crate::coin::Fee;
use crate::error::CosmosGrpcError;
use crate::msg::Msg;
use crate::private_key::PrivateKey;
use crate::private_key::TxParts;
use crate::utils::check_tx_response;
use crate::utils::determine_min_fees_and_gas;
use cosmos_sdk_proto::cosmos::bank::v1beta1::MsgSend;
use cosmos_sdk_proto::cosmos::base::abci::v1beta1::GasInfo;
use cosmos_sdk_proto::cosmos::tx::v1beta1::BroadcastMode;
use cosmos_sdk_proto::cosmos::tx::v1beta1::BroadcastTxRequest;
use cosmos_sdk_proto::cosmos::tx::v1beta1::SimulateRequest;
use cosmos_sdk_proto::cosmos::tx::v1beta1::Tx;
use cosmos_sdk_proto::cosmos::{
    base::abci::v1beta1::TxResponse, tx::v1beta1::service_client::ServiceClient as TxServiceClient,
};
use std::time::Instant;
use std::{clone::Clone, time::Duration};
use tokio::time::sleep;
use tonic::Code as TonicCode;

impl Contact {
    /// The advanced version of create_and_send transaction that expects you to
    /// perform your own signing and prep first. This is used by all message sending
    /// functions in deep_space and I suggest you use it that way as well. It provides
    /// validation for various failure conditions that is non-trivial to perform correctly.
    pub async fn send_transaction(
        &self,
        // proto serialized message for us to turn into an 'any' object
        msg: Vec<u8>,
        mode: BroadcastMode,
    ) -> Result<TxResponse, CosmosGrpcError> {
        let mut txrpc = TxServiceClient::connect(self.get_url()).await?;
        let response = txrpc
            .broadcast_tx(BroadcastTxRequest {
                tx_bytes: msg,
                mode: mode.into(),
            })
            .await?
            .into_inner()
            .tx_response
            .unwrap();
        if let Some(v) = determine_min_fees_and_gas(&response) {
            return Err(CosmosGrpcError::InsufficientFees { fee_info: v });
        } else if !check_tx_response(&response) {
            return Err(CosmosGrpcError::TransactionFailed {
                tx: response,
                time: Duration::from_secs(0),
            });
        }
        Ok(response)
    }

    pub async fn simulate_tx(
        &self,
        // proto serialized message for us to turn into an 'any' object
        tx_parts: TxParts,
    ) -> Result<GasInfo, CosmosGrpcError> {
        let mut txrpc = TxServiceClient::connect(self.get_url()).await?;

        let tx = Tx {
            body: Some(tx_parts.body),
            auth_info: Some(tx_parts.auth_info),
            signatures: tx_parts.signatures,
        };

        let response = txrpc
            .simulate(SimulateRequest { tx: Some(tx) })
            .await?
            .into_inner()
            .gas_info
            .unwrap();

        Ok(response)
    }

    /// A utility function that creates a one to one simple transaction
    /// and sends it from the provided private key, waiting the configured
    /// amount of time for the tx to enter the chain, if you do not specify
    /// a fee the smallest working amount will be selected.
    pub async fn send_tokens(
        &self,
        coin: Coin,
        fee: Option<Coin>,
        destination: Address,
        private_key: PrivateKey,
        wait_timeout: Option<Duration>,
    ) -> Result<TxResponse, CosmosGrpcError> {
        trace!("Creating transaction");
        let our_address = private_key.to_address(&self.chain_prefix).unwrap();

        let send = MsgSend {
            amount: vec![coin.into()],
            from_address: our_address.to_bech32(&self.chain_prefix).unwrap(),
            to_address: destination.to_bech32(&self.chain_prefix).unwrap(),
        };
        let msg = Msg::new("/cosmos.bank.v1beta1.MsgSend", send);

        let fee_obj = if let Some(fee) = fee {
            Fee {
                amount: vec![fee],
                gas_limit: 500_000,
                granter: None,
                payer: None,
            }
        } else {
            Fee {
                amount: vec![],
                gas_limit: 500_000,
                granter: None,
                payer: None,
            }
        };

        let args = self.get_message_args(our_address, fee_obj).await?;

        let msg_bytes = private_key.sign_std_msg(&[msg], args, MEMO)?;
        trace!("{}", msg_bytes.len());

        let response = self
            .send_transaction(msg_bytes, BroadcastMode::Sync)
            .await?;

        trace!("broadcasted! with response {:?}", response);
        if let Some(time) = wait_timeout {
            self.wait_for_tx(response, time).await
        } else {
            Ok(response)
        }
    }

    /// Utility function that waits for a tx to enter the chain by querying
    /// it's txid, will not exit for timeout time unless the error is known
    /// and unrecoverable
    pub async fn wait_for_tx(
        &self,
        response: TxResponse,
        timeout: Duration,
    ) -> Result<TxResponse, CosmosGrpcError> {
        let start = Instant::now();
        while Instant::now() - start < timeout {
            // TODO what actually determines when the tx is in the chain?
            let status = self.get_tx_by_hash(response.txhash.clone()).await;
            match status {
                Ok(status) => {
                    if let Some(res) = status.tx_response {
                        return Ok(res);
                    }
                }
                Err(CosmosGrpcError::RequestError { error }) => match error.code() {
                    TonicCode::NotFound | TonicCode::Unknown | TonicCode::InvalidArgument => {}
                    _ => {
                        return Err(CosmosGrpcError::TransactionFailed {
                            tx: response,
                            time: Instant::now() - start,
                        });
                    }
                },
                Err(e) => return Err(e),
            }
            sleep(Duration::from_secs(1)).await;
        }
        Err(CosmosGrpcError::TransactionFailed {
            tx: response,
            time: timeout,
        })
    }
}
