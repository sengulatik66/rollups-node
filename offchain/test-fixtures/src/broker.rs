// Copyright 2022 Cartesi Pte. Ltd.
//
// SPDX-License-Identifier: Apache-2.0
// Licensed under the Apache License, Version 2.0 (the "License"); you may not use
// this file except in compliance with the License. You may obtain a copy of the
// License at http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

use backoff::ExponentialBackoff;
use rollups_events::broker::{Broker, Event, INITIAL_ID};
use rollups_events::rollups_claims::{RollupsClaim, RollupsClaimsStream};
use rollups_events::rollups_inputs::{
    RollupsData, RollupsInput, RollupsInputsStream,
};
use rollups_events::HASH_SIZE;
use testcontainers::{
    clients::Cli, core::WaitFor, images::generic::GenericImage, Container,
};
use tokio::sync::Mutex;

const CHAIN_ID: u64 = 0;
const DAPP_CONTRACT_ADDRESS: [u8; 20] = [0xfa; 20];
const CONSUME_TIMEOUT: usize = 10_000; // ms

pub struct BrokerFixture<'d> {
    _node: Container<'d, GenericImage>,
    client: Mutex<Broker>,
    inputs_stream: RollupsInputsStream,
    claims_stream: RollupsClaimsStream,
    redis_endpoint: String,
    chain_id: u64,
    dapp_contract_address: [u8; 20],
}

impl BrokerFixture<'_> {
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn setup(docker: &Cli) -> BrokerFixture<'_> {
        tracing::info!("setting up redis fixture");

        tracing::trace!("starting redis docker container");
        let image = GenericImage::new("redis", "6.2").with_wait_for(
            WaitFor::message_on_stdout("Ready to accept connections"),
        );
        let node = docker.run(image);
        let port = node.get_host_port_ipv4(6379);
        let redis_endpoint = format!("redis://127.0.0.1:{}", port);
        let chain_id = CHAIN_ID;
        let dapp_contract_address = DAPP_CONTRACT_ADDRESS.to_owned();
        let backoff = ExponentialBackoff::default();
        let inputs_stream =
            RollupsInputsStream::new(chain_id, &dapp_contract_address);
        let claims_stream =
            RollupsClaimsStream::new(chain_id, &dapp_contract_address);

        tracing::trace!(
            redis_endpoint,
            "connecting to redis with rollups_events crate"
        );
        let client = Mutex::new(
            Broker::new(&redis_endpoint, backoff.clone(), CONSUME_TIMEOUT)
                .await
                .expect("failed to connect to broker"),
        );
        BrokerFixture {
            _node: node,
            client,
            inputs_stream,
            claims_stream,
            redis_endpoint,
            chain_id,
            dapp_contract_address,
        }
    }

    pub fn redis_endpoint(&self) -> &str {
        &self.redis_endpoint
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn dapp_contract_address(&self) -> &[u8; 20] {
        &self.dapp_contract_address
    }

    /// Obtain the latest event from the rollups inputs stream
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn get_latest_input_event(&self) -> Option<Event<RollupsInput>> {
        tracing::trace!("getting latest input event");
        self.client
            .lock()
            .await
            .peek_latest(&self.inputs_stream)
            .await
            .expect("failed to get latest input event")
    }

    /// Produce the input event given the data
    /// Return the produced event id
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn produce_input_event(&self, data: RollupsData) -> String {
        tracing::trace!(?data, "producing rollups-input event");
        let last_event = self.get_latest_input_event().await;
        let epoch_index = match last_event.as_ref() {
            Some(event) => match event.payload.data {
                RollupsData::AdvanceStateInput { .. } => {
                    event.payload.epoch_index
                }
                RollupsData::FinishEpoch {} => event.payload.epoch_index + 1,
            },
            None => 0,
        };
        let previous_inputs_sent_count = match last_event.as_ref() {
            Some(event) => match event.payload.data {
                RollupsData::AdvanceStateInput { .. } => {
                    event.payload.inputs_sent_count
                }
                RollupsData::FinishEpoch {} => 0,
            },
            None => 0,
        };
        let inputs_sent_count = match data {
            RollupsData::AdvanceStateInput { .. } => {
                previous_inputs_sent_count + 1
            }
            RollupsData::FinishEpoch {} => previous_inputs_sent_count,
        };
        let parent_id = match last_event {
            Some(event) => event.id,
            None => INITIAL_ID.to_owned(),
        };
        let input = RollupsInput {
            parent_id,
            epoch_index,
            inputs_sent_count,
            data,
        };
        self.produce_raw_input_event(input).await
    }

    /// Produce the input event given the input
    /// This may produce inconsistent inputs
    /// Return the produced event id
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn produce_raw_input_event(&self, input: RollupsInput) -> String {
        tracing::trace!(?input, "producing rollups-input raw event");
        self.client
            .lock()
            .await
            .produce(&self.inputs_stream, input)
            .await
            .expect("failed to produce event")
    }

    /// Produce the claim given the hash
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn produce_claim(&self, claim: [u8; HASH_SIZE]) {
        tracing::trace!(?claim, "producing rollups-claim event");
        let last_claim = self
            .client
            .lock()
            .await
            .peek_latest(&self.claims_stream)
            .await
            .expect("failed to get latest claim");
        let epoch_index = match last_claim {
            Some(event) => event.payload.epoch_index + 1,
            None => 0,
        };
        let claim = RollupsClaim { epoch_index, claim };
        self.client
            .lock()
            .await
            .produce(&self.claims_stream, claim)
            .await
            .expect("failed to produce claim");
    }

    /// Obtain all produced claims
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn consume_all_claims(&self) -> Vec<RollupsClaim> {
        tracing::trace!("consuming all rollups-claims events");
        let mut claims = vec![];
        let mut last_id = INITIAL_ID.to_owned();
        while let Some(event) = self
            .client
            .lock()
            .await
            .consume_nonblock(&self.claims_stream, &last_id)
            .await
            .expect("failed to consume claim")
        {
            claims.push(event.payload);
            last_id = event.id;
        }
        claims
    }

    /// Obtain the first n produced claims
    /// Panic in case of timeout
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn consume_n_claims(&self, n: usize) -> Vec<RollupsClaim> {
        tracing::trace!(n, "consuming n rollups-claims events");
        let mut claims = vec![];
        let mut last_id = INITIAL_ID.to_owned();
        for _ in 0..n {
            let event = self
                .client
                .lock()
                .await
                .consume_block(&self.claims_stream, &last_id)
                .await
                .expect("failed to consume claim");
            claims.push(event.payload);
            last_id = event.id
        }
        claims
    }
}