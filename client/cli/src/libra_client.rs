// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::AccountData;
use admission_control_proto::proto::AdmissionControlClientBlocking;
use anyhow::{bail, ensure, format_err, Result};
use libra_logger::prelude::*;
use libra_types::{
    access_path::AccessPath,
    account_address::AccountAddress,
    account_config::AccountResource,
    account_state_blob::{AccountStateBlob, AccountStateWithProof},
    contract_event::{ContractEvent, EventWithProof},
    crypto_proxies::LedgerInfoWithSignatures,
    get_with_proof::{
        RequestItem, ResponseItem, UpdateToLatestLedgerRequest, UpdateToLatestLedgerResponse,
    },
    transaction::{SignedTransaction, Transaction, Version},
    trusted_state::{TrustedState, TrustedStateChange},
    waypoint::Waypoint,
};
use rand::Rng;
use reqwest::blocking::Client;
use std::{convert::TryFrom, time::Duration};

const JSON_RPC_TIMEOUT_MS: u64 = 5_000;
const MAX_GRPC_RETRY_COUNT: u64 = 2;

/// A client connection to an AdmissionControl (AC) service. `LibraClient` also
/// handles verifying the server's responses, retrying on non-fatal failures, and
/// ratcheting our latest verified state, which includes the latest verified
/// version and latest verified epoch change ledger info.
///
/// ### Note
///
/// `LibraClient` will reject out-of-date responses. For example, this can happen if
///
/// 1. We make a request to the remote AC service.
/// 2. The remote service crashes and it forgets the most recent state or an
///    out-of-date replica takes its place.
/// 3. We make another request to the remote AC service. In this case, the remote
///    AC will be behind us and we will reject their response as stale.
pub struct LibraClient {
    /// The client connection to an AdmissionControl service. We will only connect
    /// when the first request is made.
    /// TODO deprecate this completely once migration to JSON RPC is complete
    client: AdmissionControlClientBlocking,
    json_rpc_client: JsonRpcClient,
    /// The latest verified chain state.
    trusted_state: TrustedState,
    /// The most recent epoch change ledger info. This is `None` if we only know
    /// about our local [`Waypoint`] and have not yet ratcheted to the remote's
    /// latest state.
    latest_epoch_change_li: Option<LedgerInfoWithSignatures>,
}

pub struct JsonRpcClient {
    addr: String,
    client: Client,
}

impl JsonRpcClient {
    pub fn new(host: &str, port: u16) -> Self {
        let addr = format!("http://{}:{}", host, port);
        let client = Client::new();

        Self { client, addr }
    }

    /// Sends JSON request `request`, performs basic checks on the payload, and returns Ok(`result`),
    /// where `result` is the payload under the key "result" in the JSON RPC response
    /// If there is an error payload in the JSON RPC response, throw an Err with message describing the error
    /// payload
    pub fn send_libra_request(
        &mut self,
        method: String,
        params: Vec<String>,
    ) -> Result<serde_json::Value> {
        let id: u64 = rand::thread_rng().gen();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        let response = self
            .send_with_retry(request)?
            .error_for_status()
            .map_err(|e| format_err!("Server returned error: {:?}", e))?;

        // check payload
        let data: serde_json::Value = response.json()?;

        // check JSON RPC protocol
        let json_rpc_protocol = data.get("jsonrpc");
        ensure!(
            json_rpc_protocol == Some(&serde_json::Value::String("2.0".to_string())),
            "JSON RPC response with incorrect protocol: {:?}",
            json_rpc_protocol
        );

        // check ID
        let response_id = data.get("id");
        ensure!(
            response_id == Some(&serde_json::json!(id)),
            "JSON RPC response ID {:?} does not match request ID {}",
            response_id,
            id
        );

        if let Some(error) = data.get("error") {
            bail!("Error in JSON RPC response: {:?}", error);
        }

        if let Some(result) = data.get("result") {
            Ok(result.clone())
        } else {
            bail!("Received JSON RPC response with no result payload");
        }
    }

    // send with retry
    pub fn send_with_retry(
        &mut self,
        request: serde_json::Value,
    ) -> Result<reqwest::blocking::Response> {
        let mut response = self.send(&request);
        let mut try_cnt = 0;

        // retry if send fails
        while try_cnt < MAX_GRPC_RETRY_COUNT && response.is_err() {
            response = self.send(&request);
            try_cnt += 1;
        }
        response
    }

    fn send(&mut self, request: &serde_json::Value) -> Result<reqwest::blocking::Response> {
        self.client
            .post(&self.addr)
            .json(request)
            .timeout(Duration::from_millis(JSON_RPC_TIMEOUT_MS))
            .send()
            .map_err(Into::into)
    }
}

impl LibraClient {
    /// Construct a new Client instance.
    // TODO(philiphayes/dmitrip): Waypoint should not be optional
    pub fn new(
        host: &str,
        ac_port: u16,
        json_rpc_port: u16,
        waypoint: Option<Waypoint>,
    ) -> Result<Self> {
        let client = AdmissionControlClientBlocking::new(host, ac_port);
        // If waypoint is present, use it for initial verification, otherwise the initial
        // verification is essentially empty.
        let initial_trusted_state = match waypoint {
            Some(waypoint) => TrustedState::from_waypoint(waypoint),
            None => TrustedState::new_trust_any_genesis_WARNING_UNSAFE(),
        };
        let json_rpc_client = JsonRpcClient::new(host, json_rpc_port);
        Ok(LibraClient {
            client,
            json_rpc_client,
            trusted_state: initial_trusted_state,
            latest_epoch_change_li: None,
        })
    }

    /// Submits a transaction and bumps the sequence number for the sender, pass in `None` for
    /// sender_account if sender's address is not managed by the client.
    pub fn submit_transaction(
        &mut self,
        sender_account_opt: Option<&mut AccountData>,
        transaction: SignedTransaction,
    ) -> Result<()> {
        // form request
        let payload = hex::encode(lcs::to_bytes(&transaction).unwrap());
        let params = vec![payload];

        match self
            .json_rpc_client
            .send_libra_request("submit".to_string(), params)
        {
            Ok(result) => {
                ensure!(
                    result == serde_json::Value::Null,
                    "Received unexpected result payload from txn submission: {:?}",
                    result
                );
                if let Some(sender_account) = sender_account_opt {
                    // Bump up sequence_number if transaction is accepted.
                    sender_account.sequence_number += 1;
                }
                Ok(())
            }
            Err(e) => {
                bail!("Transaction submission failed with error: {:?}", e);
            }
        }
    }

    fn get_with_proof(
        &mut self,
        requested_items: Vec<RequestItem>,
    ) -> Result<UpdateToLatestLedgerResponse> {
        let req =
            UpdateToLatestLedgerRequest::new(self.trusted_state.latest_version(), requested_items);

        debug!("get_with_proof with request: {:?}", req);
        let proto_req = req.clone().into();
        let resp = self.client.update_to_latest_ledger(proto_req)?;
        let resp = UpdateToLatestLedgerResponse::try_from(resp)?;

        match resp.verify(&self.trusted_state, &req)? {
            TrustedStateChange::Epoch {
                new_state,
                latest_epoch_change_li,
                latest_validator_set,
                ..
            } => {
                info!(
                    "Verified epoch change to epoch: {}, validator set: [{}]",
                    latest_epoch_change_li.ledger_info().epoch(),
                    latest_validator_set
                );
                // Update client state
                self.trusted_state = new_state;
                self.latest_epoch_change_li = Some(latest_epoch_change_li.clone());
            }
            TrustedStateChange::Version { new_state, .. } => {
                self.trusted_state = new_state;
            }
        }

        Ok(resp)
    }

    fn need_to_retry<T>(try_cnt: u64, ret: &Result<T>) -> bool {
        if try_cnt >= MAX_GRPC_RETRY_COUNT {
            return false;
        }

        if let Err(error) = ret {
            if let Some(grpc_error) = error.downcast_ref::<tonic::Status>() {
                // Only retry when the connection is down to make sure we won't
                // send one txn twice.
                return grpc_error.code() == tonic::Code::Unavailable
                    || grpc_error.code() == tonic::Code::Unknown;
            }
        }

        false
    }

    /// LedgerInfo corresponding to the latest epoch change.
    pub(crate) fn latest_epoch_change_li(&self) -> Option<&LedgerInfoWithSignatures> {
        self.latest_epoch_change_li.as_ref()
    }

    /// Sync version of get_with_proof
    pub(crate) fn get_with_proof_sync(
        &mut self,
        requested_items: Vec<RequestItem>,
    ) -> Result<UpdateToLatestLedgerResponse> {
        let mut resp = self.get_with_proof(requested_items.clone());

        let mut try_cnt = 0;
        while Self::need_to_retry(try_cnt, &resp) {
            resp = self.get_with_proof(requested_items.clone());
            try_cnt += 1;
        }

        resp
    }

    /// Get the latest account sequence number for the account specified.
    pub fn get_sequence_number(&mut self, address: AccountAddress) -> Result<u64> {
        Ok(match self.get_account_blob(address)?.0 {
            Some(blob) => AccountResource::try_from(&blob)?.sequence_number(),
            None => 0,
        })
    }

    /// Get the latest account state blob from validator.
    pub(crate) fn get_account_blob(
        &mut self,
        address: AccountAddress,
    ) -> Result<(Option<AccountStateBlob>, Version)> {
        let req_item = RequestItem::GetAccountState { address };

        let mut response = self.get_with_proof_sync(vec![req_item])?;
        let account_state_with_proof = response
            .response_items
            .remove(0)
            .into_get_account_state_response()?;

        Ok((
            account_state_with_proof.blob,
            response.ledger_info_with_sigs.ledger_info().version(),
        ))
    }

    /// Get transaction from validator by account and sequence number.
    pub fn get_txn_by_acc_seq(
        &mut self,
        account: AccountAddress,
        sequence_number: u64,
        fetch_events: bool,
    ) -> Result<Option<(Transaction, Option<Vec<ContractEvent>>)>> {
        let req_item = RequestItem::GetAccountTransactionBySequenceNumber {
            account,
            sequence_number,
            fetch_events,
        };

        let mut response = self.get_with_proof_sync(vec![req_item])?;
        let (txn_with_proof, _) = response
            .response_items
            .remove(0)
            .into_get_account_txn_by_seq_num_response()?;

        Ok(txn_with_proof.map(|t| (t.transaction, t.events)))
    }

    /// Get transactions in range (start_version..start_version + limit - 1) from validator.
    pub fn get_txn_by_range(
        &mut self,
        start_version: u64,
        limit: u64,
        fetch_events: bool,
    ) -> Result<Vec<(Transaction, Option<Vec<ContractEvent>>)>> {
        // Make the request.
        let req_item = RequestItem::GetTransactions {
            start_version,
            limit,
            fetch_events,
        };
        let mut response = self.get_with_proof_sync(vec![req_item])?;
        let txn_list_with_proof = response
            .response_items
            .remove(0)
            .into_get_transactions_response()?;

        // Transform the response.
        let num_txns = txn_list_with_proof.transactions.len();
        let event_lists = txn_list_with_proof
            .events
            .map(|event_lists| event_lists.into_iter().map(Some).collect())
            .unwrap_or_else(|| vec![None; num_txns]);

        Ok(itertools::zip_eq(txn_list_with_proof.transactions, event_lists).collect())
    }

    /// Get event by access path from validator. AccountStateWithProof will be returned if
    /// 1. No event is available. 2. Ascending and available event number < limit.
    /// 3. Descending and start_seq_num > latest account event sequence number.
    pub fn get_events_by_access_path(
        &mut self,
        access_path: AccessPath,
        start_event_seq_num: u64,
        ascending: bool,
        limit: u64,
    ) -> Result<(Vec<EventWithProof>, AccountStateWithProof)> {
        let req_item = RequestItem::GetEventsByEventAccessPath {
            access_path,
            start_event_seq_num,
            ascending,
            limit,
        };

        let mut response = self.get_with_proof_sync(vec![req_item])?;
        let value_with_proof = response.response_items.remove(0);
        match value_with_proof {
            ResponseItem::GetEventsByEventAccessPath {
                events_with_proof,
                proof_of_latest_event,
            } => Ok((events_with_proof, proof_of_latest_event)),
            _ => bail!(
                "Incorrect type of response returned: {:?}",
                value_with_proof
            ),
        }
    }
}
