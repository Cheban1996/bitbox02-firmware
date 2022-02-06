// Copyright 2022 Shift Crypto AG
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::pb;
use super::Error;

use super::script::serialize_varint;

use crate::workflow::status;

use alloc::vec::Vec;

use pb::request::Request;
use pb::response::Response;

use prost::Message;

use pb::btc_sign_next_response::Type as NextType;

use sha2::{Digest, Sha256};

fn encode<M: Message>(msg: &M) -> Vec<u8> {
    let mut serialized = Vec::<u8>::new();
    msg.encode(&mut serialized).unwrap();
    serialized
}

/// After each request from the host, we send a `BtcSignNextResponse` response back to the host,
/// containing information which request we want next, and containing additional metadata if
/// available (e.g. a signature after signing an input).
struct NextResponse {
    next: pb::BtcSignNextResponse,
    /// If true, `next` is wrapped in the `BTCResponse` protobuf message, otherwise it is sent
    /// directly in a `Response` message.
    wrap: bool,
}

impl NextResponse {
    fn to_protobuf(&self) -> Response {
        if self.wrap {
            Response::Btc(pb::BtcResponse {
                response: Some(pb::btc_response::Response::SignNext(self.next.clone())),
            })
        } else {
            Response::BtcSignNext(self.next.clone())
        }
    }
}

/// Wait for the next request sent by the host. Since host<->device communication is a
/// request/response pattern, we have to send a response (to the previous request) before getting
/// the next request.
///
/// In BTC signing, the response is always a `BtcSignNextResponse`, but depending on the previous
/// request, it is either a direct response result (hww.proto:Response), or a a result wrapped in a
/// `BTCResponse` (which was introduced latter, hence the messages are scattered). `response.wrap`
/// is set so the next call to this function wraps the response correctly.
///
/// The NextResponse contains information for the host as to which request we need, but also
/// additional results, e.g. a signature after an input is signed. The response is reset to default
/// values after this call so that this additional data is only sent once.
async fn get_request(
    typ: NextType,
    index: u32,
    prev_index: Option<u32>,
    response: &mut NextResponse,
) -> Result<Request, Error> {
    response.next.r#type = typ as _;
    response.next.index = index;
    if let Some(prev_index) = prev_index {
        response.next.prev_index = prev_index;
    }
    let request = crate::hww::next_request(response.to_protobuf()).await?;
    response.next = pb::BtcSignNextResponse {
        r#type: 0,
        index: 0,
        has_signature: false,
        signature: vec![],
        prev_index: 0,
        anti_klepto_signer_commitment: None,
    };
    Ok(request)
}

async fn get_tx_input(
    index: u32,
    response: &mut NextResponse,
) -> Result<pb::BtcSignInputRequest, Error> {
    let request = get_request(NextType::Input, index, None, response).await?;
    response.wrap = false;
    match request {
        Request::BtcSignInput(request) => Ok(request),
        _ => Err(Error::InvalidState),
    }
}

async fn get_prevtx_init(
    index: u32,
    response: &mut NextResponse,
) -> Result<pb::BtcPrevTxInitRequest, Error> {
    response.next.r#type = NextType::PrevtxInit as _;
    response.next.index = index;
    let request = get_request(NextType::PrevtxInit, index, None, response).await?;
    response.wrap = true;
    match request {
        Request::Btc(pb::BtcRequest {
            request: Some(pb::btc_request::Request::PrevtxInit(request)),
        }) => Ok(request),
        _ => Err(Error::InvalidState),
    }
}

async fn get_prevtx_input(
    input_index: u32,
    prevtx_input_index: u32,
    response: &mut NextResponse,
) -> Result<pb::BtcPrevTxInputRequest, Error> {
    let request = get_request(
        NextType::PrevtxInput,
        input_index,
        Some(prevtx_input_index),
        response,
    )
    .await?;
    response.wrap = true;
    match request {
        Request::Btc(pb::BtcRequest {
            request: Some(pb::btc_request::Request::PrevtxInput(request)),
        }) => Ok(request),
        _ => Err(Error::InvalidState),
    }
}

async fn get_prevtx_output(
    output_index: u32,
    prevtx_output_index: u32,
    response: &mut NextResponse,
) -> Result<pb::BtcPrevTxOutputRequest, Error> {
    let request = get_request(
        NextType::PrevtxOutput,
        output_index,
        Some(prevtx_output_index),
        response,
    )
    .await?;
    response.wrap = true;
    match request {
        Request::Btc(pb::BtcRequest {
            request: Some(pb::btc_request::Request::PrevtxOutput(request)),
        }) => Ok(request),
        _ => Err(Error::InvalidState),
    }
}

async fn get_tx_output(
    index: u32,
    response: &mut NextResponse,
) -> Result<pb::BtcSignOutputRequest, Error> {
    let request = get_request(NextType::Output, index, None, response).await?;
    response.wrap = false;
    match request {
        Request::BtcSignOutput(request) => Ok(request),
        _ => Err(Error::InvalidState),
    }
}

async fn get_antiklepto_host_nonce(
    index: u32,
    response: &mut NextResponse,
) -> Result<pb::AntiKleptoSignatureRequest, Error> {
    let request = get_request(NextType::HostNonce, index, None, response).await?;
    response.wrap = true;
    match request {
        Request::Btc(pb::BtcRequest {
            request: Some(pb::btc_request::Request::AntikleptoSignature(request)),
        }) => Ok(request),
        _ => Err(Error::InvalidState),
    }
}

/// Stream an input's previous transaction and verify that the prev_out_hash in the input matches
/// the hash of the previous transaction, as well as that the amount provided in the input is correct.
async fn handle_prevtx(
    input_index: u32,
    input: &pb::BtcSignInputRequest,
    num_inputs: u32,
    progress_component: &mut bitbox02::ui::Component<'_>,
    next_response: &mut NextResponse,
) -> Result<(), Error> {
    let prevtx_init = get_prevtx_init(input_index, next_response).await?;

    if prevtx_init.num_inputs < 1 || prevtx_init.num_outputs < 1 {
        return Err(Error::InvalidInput);
    }

    let mut hasher = Sha256::new();
    hasher.update(prevtx_init.version.to_le_bytes());

    hasher.update(serialize_varint(prevtx_init.num_inputs as u64).as_slice());
    for prevtx_input_index in 0..prevtx_init.num_inputs {
        // Update progress.
        bitbox02::ui::progress_set(progress_component, {
            let step = 1f32 / (num_inputs as f32);
            let subprogress: f32 = (prevtx_input_index as f32)
                / (prevtx_init.num_inputs + prevtx_init.num_outputs) as f32;
            (input_index as f32 + subprogress) * step
        });

        let prevtx_input = get_prevtx_input(input_index, prevtx_input_index, next_response).await?;
        hasher.update(prevtx_input.prev_out_hash.as_slice());
        hasher.update(prevtx_input.prev_out_index.to_le_bytes());
        hasher.update(serialize_varint(prevtx_input.signature_script.len() as u64).as_slice());
        hasher.update(prevtx_input.signature_script.as_slice());
        hasher.update(prevtx_input.sequence.to_le_bytes());
    }

    hasher.update(serialize_varint(prevtx_init.num_outputs as u64).as_slice());
    for prevtx_output_index in 0..prevtx_init.num_outputs {
        // Update progress.
        bitbox02::ui::progress_set(progress_component, {
            let step = 1f32 / (num_inputs as f32);
            let subprogress: f32 = (prevtx_init.num_inputs + prevtx_output_index) as f32
                / (prevtx_init.num_inputs + prevtx_init.num_outputs) as f32;
            (input_index as f32 + subprogress) * step
        });

        let prevtx_output =
            get_prevtx_output(input_index, prevtx_output_index, next_response).await?;
        if prevtx_output_index == input.prev_out_index
            && input.prev_out_value != prevtx_output.value
        {
            return Err(Error::InvalidInput);
        }
        hasher.update(prevtx_output.value.to_le_bytes());
        hasher.update(serialize_varint(prevtx_output.pubkey_script.len() as u64).as_slice());
        hasher.update(prevtx_output.pubkey_script.as_slice());
    }

    hasher.update(prevtx_init.locktime.to_le_bytes());
    // Hash again to produce the final double-hash.
    let hash = Sha256::digest(&hasher.finalize());
    if hash.as_slice() != input.prev_out_hash.as_slice() {
        return Err(Error::InvalidInput);
    }
    Ok(())
}

/// Singing flow:
///
/// init
/// for each input:
///    inputs_pass1
///    prevtx init
///    for each prevtx input:
///        prevtx inputs
///    for each prevtx output:
///        prevtx outputs
/// for each output:
///    outputs
/// for each input:
///    inputs_pass2
///    if input contains a host nonce commitment, the anti-klepto protocol is active:
///       inputs_pass2_antiklepto_host_nonce
///
/// The hash_prevout and hash_sequence and total_in are accumulated in inputs_pass1.
///
/// For each input in pass1, the input's prevtx is streamed to compute and compare the prevOutHash
/// and input amount.
///
/// For each output, the recipient is confirmed. At the last output, the total out, fee, locktime/RBF
/// are confirmed.
///
/// The inputs are signed in inputs_pass2.
///
/// IMPORTANT assumptions:
///
/// - In the 2nd pass, if the inputs provided by the host are not the same as in the 1st pass,
///   nothing bad will happen because the sighash uses the prevout and sequence hashes from the first
///   pass, and the value from the 2nd pass. The BTC consensus rules will reject the tx if there is a
///   mismatch.
///
/// - Only SIGHASH_ALL. Other sighash types must be carefully studied and might not be secure with
///   the above flow or the above assumption.
async fn _process(request: &pb::BtcSignInitRequest) -> Result<Response, Error> {
    if bitbox02::keystore::is_locked() {
        return Err(Error::InvalidState);
    }
    bitbox02::app_btc::sign_init_wrapper(encode(request).as_ref())?;

    let mut progress_component = {
        let mut c = bitbox02::ui::progress_create("Loading transaction...");
        c.screen_stack_push();
        Some(c)
    };

    let mut next_response = NextResponse {
        next: pb::BtcSignNextResponse {
            r#type: 0,
            index: 0,
            has_signature: false,
            signature: vec![],
            prev_index: 0,
            anti_klepto_signer_commitment: None,
        },
        wrap: false,
    };
    for input_index in 0..request.num_inputs {
        // Update progress.
        bitbox02::ui::progress_set(
            progress_component.as_mut().unwrap(),
            (input_index as f32) / (request.num_inputs as f32),
        );

        let tx_input = get_tx_input(input_index, &mut next_response).await?;
        let last = input_index == request.num_inputs - 1;
        bitbox02::app_btc::sign_input_pass1_wrapper(encode(&tx_input).as_ref(), last)?;
        handle_prevtx(
            input_index,
            &tx_input,
            request.num_inputs,
            progress_component.as_mut().unwrap(),
            &mut next_response,
        )
        .await?;
    }

    // The progress for loading the inputs is 100%.
    bitbox02::ui::progress_set(progress_component.as_mut().unwrap(), 1.);

    // Base component on the screen stack during signing, which is shown while the device is waiting
    // for the next signing api call. Without this, the 'See the BitBoxApp' waiting screen would
    // flicker in between user confirmations. All user input happens during output processing.
    //
    // We only start rendering this (and stop rendering the inputs progress bar) after we receive
    // the first output, otherwise there is a noticable delay between processing the last input and
    // receiving the first output.
    let mut empty_component = None;

    for output_index in 0..request.num_outputs {
        let tx_output = get_tx_output(output_index, &mut next_response).await?;
        if output_index == 0 {
            // Stop rendering inputs progress update.
            drop(progress_component.take());

            empty_component = {
                let mut c = bitbox02::ui::empty_create();
                c.screen_stack_push();
                Some(c)
            };
        }
        let last = output_index == request.num_outputs - 1;
        bitbox02::app_btc::sign_output_wrapper(encode(&tx_output).as_ref(), last)?;
    }

    status::status("Transaction\nconfirmed", true).await;

    // Stop rendering the empty component.
    drop(empty_component);

    // Show progress of signing inputs if there are more than 2 inputs. This is an arbitrary cutoff;
    // less or equal to 2 inputs is fast enough so it does not need a progress bar.
    let mut progress_component = if request.num_inputs > 2 {
        let mut c = bitbox02::ui::progress_create("Signing transaction...");
        c.screen_stack_push();
        Some(c)
    } else {
        None
    };

    for input_index in 0..request.num_inputs {
        let tx_input = get_tx_input(input_index, &mut next_response).await?;
        let last = input_index == request.num_inputs - 1;
        let (signature, anti_klepto_signer_commitment) =
            bitbox02::app_btc::sign_input_pass2_wrapper(encode(&tx_input).as_ref(), last)?;
        // Engage in the Anti-Klepto protocol if the host sends a host nonce commitment.
        if tx_input.host_nonce_commitment.is_some() {
            next_response.next.anti_klepto_signer_commitment =
                Some(pb::AntiKleptoSignerCommitment {
                    commitment: anti_klepto_signer_commitment,
                });

            let antiklepto_host_nonce =
                get_antiklepto_host_nonce(input_index, &mut next_response).await?;

            next_response.next.has_signature = true;
            next_response.next.signature = bitbox02::app_btc::sign_antiklepto_wrapper(
                encode(&antiklepto_host_nonce).as_ref(),
            )?;
        } else {
            next_response.next.has_signature = true;
            next_response.next.signature = signature;
        }

        // Update progress.
        if let Some(ref mut c) = progress_component {
            bitbox02::ui::progress_set(c, (input_index + 1) as f32 / (request.num_inputs as f32));
        }
    }

    next_response.next.r#type = NextType::Done as _;
    Ok(next_response.to_protobuf())
}

pub async fn process(request: &pb::BtcSignInitRequest) -> Result<Response, Error> {
    let result = _process(request).await;
    bitbox02::app_btc::sign_reset();
    if let Err(Error::UserAbort) = result {
        status::status("Transaction\ncanceled", false).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bb02_async::block_on;
    use alloc::boxed::Box;
    use bitbox02::testing::{mock, mock_unlocked, Data};
    use util::bip32::HARDENED;

    struct TxInput {
        input: pb::BtcSignInputRequest,
        prevtx_version: u32,
        prevtx_inputs: Vec<pb::BtcPrevTxInputRequest>,
        prevtx_outputs: Vec<pb::BtcPrevTxOutputRequest>,
        prevtx_locktime: u32,
    }

    struct Transaction {
        coin: pb::BtcCoin,
        // How many dialogs the user has to confirm in the test transaction
        total_confirmations: u32,
        version: u32,
        inputs: Vec<TxInput>,
        outputs: Vec<pb::BtcSignOutputRequest>,
        locktime: u32,
    }

    impl Transaction {
        /// An arbitrary test transaction with some inputs and outputs.
        fn new(coin: pb::BtcCoin) -> Self {
            let bip44_coin = super::super::params::get(coin).bip44_coin;
            Transaction {
                coin,
                total_confirmations: 6,
                version: 1,
                inputs: vec![
                    TxInput {
                        input: pb::BtcSignInputRequest {
                            prev_out_hash: vec![
                                0x45, 0x17, 0x74, 0x50, 0x1b, 0xaf, 0xdf, 0xf7, 0x46, 0x9, 0xe,
                                0x6, 0x16, 0xd9, 0x5e, 0xd0, 0x80, 0xd7, 0x82, 0x9a, 0xfe, 0xa2,
                                0xbd, 0x97, 0x8a, 0xf8, 0x11, 0xf4, 0x5e, 0x43, 0x81, 0x39,
                            ],
                            prev_out_index: 1,
                            prev_out_value: 1010000000,
                            sequence: 0xffffffff,
                            keypath: vec![84 + HARDENED, bip44_coin, 10 + HARDENED, 0, 5],
                            script_config_index: 0,
                            host_nonce_commitment: None,
                        },
                        prevtx_version: 1,
                        prevtx_inputs: vec![
                            pb::BtcPrevTxInputRequest {
                                prev_out_hash: vec![
                                    0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74,
                                    0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74,
                                    0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74,
                                    0x74, 0x74,
                                ],
                                prev_out_index: 3,
                                signature_script: b"signature script".to_vec(),
                                sequence: 0xffffffff - 2,
                            },
                            pb::BtcPrevTxInputRequest {
                                prev_out_hash: vec![
                                    0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75,
                                    0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75,
                                    0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75, 0x75,
                                    0x75, 0x75,
                                ],
                                prev_out_index: 23,
                                signature_script: b"signature script 2".to_vec(),
                                sequence: 123456,
                            },
                        ],
                        prevtx_outputs: vec![
                            pb::BtcPrevTxOutputRequest {
                                value: 101000000, // btc 1.01
                                pubkey_script: b"pubkey script".to_vec(),
                            },
                            pb::BtcPrevTxOutputRequest {
                                value: 1010000000, // btc 10.1
                                pubkey_script: b"pubkey script 2".to_vec(),
                            },
                        ],
                        prevtx_locktime: 0,
                    },
                    TxInput {
                        input: pb::BtcSignInputRequest {
                            prev_out_hash: vec![
                                0x40, 0x9b, 0x4f, 0x56, 0xca, 0x9f, 0x6, 0xcb, 0x88, 0x28, 0x3,
                                0xad, 0x55, 0x4b, 0xeb, 0x1d, 0x9e, 0xf8, 0x78, 0x7, 0xf0, 0x52,
                                0x29, 0xe7, 0x55, 0x15, 0xe4, 0xb2, 0xaa, 0x87, 0x69, 0x1d,
                            ],
                            prev_out_index: 0,
                            prev_out_value: 1020000000, // btc 10.2, matches prevout tx output at index 0.
                            sequence: 0xffffffff,
                            keypath: vec![84 + HARDENED, bip44_coin, 10 + HARDENED, 0, 7],
                            script_config_index: 0,
                            host_nonce_commitment: None,
                        },
                        prevtx_version: 2,
                        prevtx_inputs: vec![pb::BtcPrevTxInputRequest {
                            prev_out_hash: vec![
                                0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74,
                                0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74,
                                0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74, 0x74,
                            ],
                            prev_out_index: 3,
                            signature_script: b"signature script".to_vec(),
                            sequence: 0xffffffff - 2,
                        }],
                        prevtx_outputs: vec![pb::BtcPrevTxOutputRequest {
                            value: 1020000000, // btc 10.2
                            pubkey_script: b"pubkey script".to_vec(),
                        }],
                        prevtx_locktime: 87654,
                    },
                ],
                outputs: vec![
                    pb::BtcSignOutputRequest {
                        ours: false,
                        r#type: pb::BtcOutputType::P2pkh as _,
                        value: 100000000, // btc 1,
                        payload: vec![
                            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
                            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
                        ],
                        keypath: vec![],
                        script_config_index: 0,
                    },
                    pb::BtcSignOutputRequest {
                        ours: false,
                        r#type: pb::BtcOutputType::P2sh as _,
                        value: 1234567890, // btc 12.3456789,
                        payload: vec![
                            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
                            0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
                        ],
                        keypath: vec![],
                        script_config_index: 0,
                    },
                    pb::BtcSignOutputRequest {
                        ours: false,
                        r#type: pb::BtcOutputType::P2wpkh as _,
                        value: 6000, // btc .00006
                        payload: vec![
                            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
                            0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
                        ],
                        keypath: vec![],
                        script_config_index: 0,
                    },
                    pb::BtcSignOutputRequest {
                        ours: false,
                        r#type: pb::BtcOutputType::P2wsh as _,
                        value: 7000, // btc .00007
                        payload: vec![
                            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
                            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
                            0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44,
                        ],
                        keypath: vec![],
                        script_config_index: 0,
                    },
                    pb::BtcSignOutputRequest {
                        // change
                        ours: true,
                        r#type: 0,
                        value: 690000000, // btc 6.9
                        payload: vec![],
                        keypath: vec![84 + HARDENED, bip44_coin, 10 + HARDENED, 1, 3],
                        script_config_index: 0,
                    },
                    pb::BtcSignOutputRequest {
                        // change #2
                        ours: true,
                        r#type: 0,
                        value: 100,
                        payload: vec![],
                        keypath: vec![84 + HARDENED, bip44_coin, 10 + HARDENED, 1, 30],
                        script_config_index: 0,
                    },
                ],
                locktime: 0,
            }
        }

        fn init_request(&self) -> pb::BtcSignInitRequest {
            pb::BtcSignInitRequest {
                coin: self.coin as _,
                script_configs: vec![pb::BtcScriptConfigWithKeypath {
                    script_config: Some(pb::BtcScriptConfig {
                        config: Some(pb::btc_script_config::Config::SimpleType(
                            pb::btc_script_config::SimpleType::P2wpkh as _,
                        )),
                    }),
                    keypath: vec![
                        84 + HARDENED,
                        super::super::params::get(self.coin).bip44_coin,
                        10 + HARDENED,
                    ],
                }],
                version: self.version,
                num_inputs: self.inputs.len() as _,
                num_outputs: self.outputs.len() as _,
                locktime: self.locktime,
            }
        }

        /// Return the transaction part requested by the device.
        fn make_host_request(&self, response: Response) -> Request {
            let next: pb::BtcSignNextResponse = match response {
                Response::BtcSignNext(next) => next,
                Response::Btc(pb::BtcResponse {
                    response: Some(pb::btc_response::Response::SignNext(next)),
                }) => next,
                _ => panic!("wrong response type"),
            };
            match NextType::from_i32(next.r#type).unwrap() {
                NextType::Input => {
                    Request::BtcSignInput(self.inputs[next.index as usize].input.clone())
                }
                NextType::Output => {
                    Request::BtcSignOutput(self.outputs[next.index as usize].clone())
                }
                NextType::PrevtxInit => Request::Btc(pb::BtcRequest {
                    request: Some(pb::btc_request::Request::PrevtxInit(
                        pb::BtcPrevTxInitRequest {
                            version: self.inputs[next.index as usize].prevtx_version,
                            num_inputs: self.inputs[next.index as usize].prevtx_inputs.len() as _,
                            num_outputs: self.inputs[next.index as usize].prevtx_outputs.len() as _,
                            locktime: self.inputs[next.index as usize].prevtx_locktime,
                        },
                    )),
                }),
                NextType::PrevtxInput => Request::Btc(pb::BtcRequest {
                    request: Some(pb::btc_request::Request::PrevtxInput(
                        self.inputs[next.index as usize].prevtx_inputs[next.prev_index as usize]
                            .clone(),
                    )),
                }),
                NextType::PrevtxOutput => Request::Btc(pb::BtcRequest {
                    request: Some(pb::btc_request::Request::PrevtxOutput(
                        self.inputs[next.index as usize].prevtx_outputs[next.prev_index as usize]
                            .clone(),
                    )),
                }),
                _ => panic!("unexpected next response"),
            }
        }
    }

    fn mock_host_responder(tx: alloc::rc::Rc<core::cell::RefCell<Transaction>>) {
        *crate::hww::MOCK_NEXT_REQUEST.0.borrow_mut() =
            Some(Box::new(move |response: Response| {
                Ok(tx.borrow().make_host_request(response))
            }));
    }

    /// Pass/accept all user confirmations.
    fn mock_default_ui() {
        bitbox02::app_btc_sign_ui::mock(bitbox02::app_btc_sign_ui::Ui {
            verify_recipient: Box::new(|_recipient, _amount| true),
            confirm: Box::new(|_title, _body| true),
            verify_total: Box::new(|_total, _fee| true),
        });
    }

    #[test]
    pub fn test_sign_init_fail() {
        let init_req_valid = pb::BtcSignInitRequest {
            coin: pb::BtcCoin::Btc as _,
            script_configs: vec![pb::BtcScriptConfigWithKeypath {
                script_config: Some(pb::BtcScriptConfig {
                    config: Some(pb::btc_script_config::Config::SimpleType(
                        pb::btc_script_config::SimpleType::P2wpkh as _,
                    )),
                }),
                keypath: vec![84 + HARDENED, 0 + HARDENED, 10 + HARDENED],
            }],
            version: 1,
            num_inputs: 1,
            num_outputs: 1,
            locktime: 0,
        };

        {
            // test keystore locked
            bitbox02::keystore::lock();
            assert_eq!(block_on(process(&init_req_valid)), Err(Error::InvalidState));
        }

        mock_unlocked();

        {
            // test invalid version
            let mut init_req_invalid = init_req_valid.clone();
            for version in 3..10 {
                init_req_invalid.version = version;
                assert_eq!(
                    block_on(process(&init_req_invalid)),
                    Err(Error::InvalidInput)
                );
            }
        }
        {
            // test invalid locktime
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.locktime = 500000000;
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }
        {
            // test invalid inputs
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.num_inputs = 0;
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }
        {
            // test invalid outputs
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.num_outputs = 0;
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }
        {
            // test invalid coin
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.coin = 4; // BtcCoin is defined from 0 to 3.
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }
        {
            // test invalid account keypath
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.script_configs[0].keypath[2] = HARDENED + 100;
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }
        {
            // no script configs is invalid
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.script_configs = vec![];
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }
        {
            // can't mix script configs from different bip44 accounts
            // (mixing input scripts is allowed, but only from the same bip44 account).
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.script_configs = vec![
                pb::BtcScriptConfigWithKeypath {
                    script_config: Some(pb::BtcScriptConfig {
                        config: Some(pb::btc_script_config::Config::SimpleType(
                            pb::btc_script_config::SimpleType::P2wpkh as _,
                        )),
                    }),
                    keypath: vec![84 + HARDENED, 0 + HARDENED, 10 + HARDENED],
                },
                pb::BtcScriptConfigWithKeypath {
                    script_config: Some(pb::BtcScriptConfig {
                        config: Some(pb::btc_script_config::Config::SimpleType(
                            pb::btc_script_config::SimpleType::P2wpkhP2sh as _,
                        )),
                    }),
                    keypath: vec![49 + HARDENED, 0 + HARDENED, 0 + HARDENED],
                },
            ];
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }

        {
            // can't mix simple type (singlesig) and multisig configs in one tx
            let mut init_req_invalid = init_req_valid.clone();
            init_req_invalid.script_configs = vec![
                pb::BtcScriptConfigWithKeypath {
                    script_config: Some(pb::BtcScriptConfig {
                        config: Some(pb::btc_script_config::Config::SimpleType(
                            pb::btc_script_config::SimpleType::P2wpkh as _,
                        )),
                    }),
                    keypath: vec![84 + HARDENED, 0 + HARDENED, 10 + HARDENED],
                },
                pb::BtcScriptConfigWithKeypath {
                    script_config: Some(pb::BtcScriptConfig {
                        config: Some(pb::btc_script_config::Config::Multisig(
                            pb::btc_script_config::Multisig {
                                threshold: 1,
                                xpubs: vec![
                                    pb::XPub {
                                        ..Default::default()
                                    },
                                    pb::XPub {
                                        ..Default::default()
                                    },
                                ],
                                our_xpub_index: 0,
                                script_type: pb::btc_script_config::multisig::ScriptType::P2wsh
                                    as _,
                            },
                        )),
                    }),
                    keypath: vec![49 + HARDENED, 0 + HARDENED, 0 + HARDENED],
                },
            ];
            assert_eq!(
                block_on(process(&init_req_invalid)),
                Err(Error::InvalidInput)
            );
        }
    }

    #[test]
    pub fn test_process() {
        static mut UI_COUNTER: u32 = 0;
        for coin in &[pb::BtcCoin::Btc, pb::BtcCoin::Ltc] {
            let transaction = alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(*coin)));

            let tx = transaction.clone();
            mock_host_responder(tx);
            mock_unlocked();
            unsafe { UI_COUNTER = 0 }
            bitbox02::app_btc_sign_ui::mock(bitbox02::app_btc_sign_ui::Ui {
                verify_recipient: Box::new(move |recipient, amount| {
                    match unsafe {
                        UI_COUNTER += 1;
                        UI_COUNTER
                    } {
                        1 => {
                            match coin {
                                &pb::BtcCoin::Btc => {
                                    assert_eq!(recipient, "12ZEw5Hcv1hTb6YUQJ69y1V7uhcoDz92PH");
                                    assert_eq!(amount, "1 BTC");
                                }
                                &pb::BtcCoin::Ltc => {
                                    assert_eq!(recipient, "LLnCCHbSzfwWquEdaS5TF2Yt7uz5Qb1SZ1");
                                    assert_eq!(amount, "1 LTC");
                                }
                                _ => panic!("unexpected coin"),
                            }
                            true
                        }
                        2 => {
                            match coin {
                                &pb::BtcCoin::Btc => {
                                    assert_eq!(recipient, "34oVnh4gNviJGMnNvgquMeLAxvXJuaRVMZ");
                                    assert_eq!(amount, "12.3456789 BTC");
                                }
                                &pb::BtcCoin::Ltc => {
                                    assert_eq!(recipient, "MB1e6aUeL3Zj4s4H2ZqFBHaaHd7kvvzTco");
                                    assert_eq!(amount, "12.3456789 LTC");
                                }
                                _ => panic!("unexpected coin"),
                            }
                            true
                        }
                        3 => {
                            match coin {
                                &pb::BtcCoin::Btc => {
                                    assert_eq!(
                                        recipient,
                                        "bc1qxvenxvenxvenxvenxvenxvenxvenxven2ymjt8"
                                    );
                                    assert_eq!(amount, "0.00006 BTC");
                                }
                                &pb::BtcCoin::Ltc => {
                                    assert_eq!(
                                        recipient,
                                        "ltc1qxvenxvenxvenxvenxvenxvenxvenxvenwcpknh"
                                    );
                                    assert_eq!(amount, "0.00006 LTC");
                                }
                                _ => panic!("unexpected coin"),
                            }
                            true
                        }
                        4 => {
                            match coin {
                                &pb::BtcCoin::Btc => {
                                    assert_eq!(
                                        recipient,
                                        "bc1qg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zqd8sxw4"
                                    );
                                    assert_eq!(amount, "0.00007 BTC");
                                }
                                &pb::BtcCoin::Ltc => {
                                    assert_eq!(
                                        recipient,
                                        "ltc1qg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zyg3zqwr7k5s"
                                    );
                                    assert_eq!(amount, "0.00007 LTC");
                                }
                                _ => panic!("unexpected coin"),
                            }
                            true
                        }
                        _ => panic!("unexpected UI dialog"),
                    }
                }),
                confirm: Box::new(|title, body| {
                    match unsafe {
                        UI_COUNTER += 1;
                        UI_COUNTER
                    } {
                        5 => {
                            assert_eq!(title, "Warning");
                            assert_eq!(body, "There are 2\nchange outputs.\nProceed?");
                            true
                        }
                        _ => panic!("unexpected UI dialog"),
                    }
                }),
                verify_total: Box::new(move |total, fee| {
                    match unsafe {
                        UI_COUNTER += 1;
                        UI_COUNTER
                    } {
                        6 => {
                            match coin {
                                &pb::BtcCoin::Btc => {
                                    assert_eq!(total, "13.399999 BTC");
                                    assert_eq!(fee, "0.0541901 BTC");
                                }
                                &pb::BtcCoin::Ltc => {
                                    assert_eq!(total, "13.399999 LTC");
                                    assert_eq!(fee, "0.0541901 LTC");
                                }
                                _ => panic!("unexpected coin"),
                            }
                            true
                        }
                        _ => panic!("unexpected UI dialog"),
                    }
                }),
            });
            let tx = transaction.borrow();
            let result = block_on(process(&tx.init_request()));
            match result {
                Ok(Response::BtcSignNext(next)) => {
                    assert!(next.has_signature);
                    match coin {
                        &pb::BtcCoin::Btc => {
                            assert_eq!(
                                &next.signature,
                                b"\x2e\x08\x4a\x0a\x5f\x9b\xab\xb3\x5d\xf6\xec\x3a\x89\x72\x0b\xcf\xc0\x88\xd4\xba\x6a\xee\x47\x97\x3c\x55\xfe\xc3\xb3\xdd\xaa\x60\x07\xc7\xb1\x1c\x8b\x5a\x1a\x68\x20\xca\x74\xa8\x5a\xeb\x4c\xf5\x45\xc1\xb3\x37\x53\x70\xf4\x4f\x24\xd5\x3d\x61\xfe\x67\x6e\x4c");
                        }
                        _ => {}
                    }
                }
                _ => panic!("wrong result"),
            }
            assert_eq!(unsafe { UI_COUNTER }, tx.total_confirmations);
        }
    }

    /// Test that receiving an unexpected message from the host results in an invalid state error.
    #[test]
    pub fn test_invalid_state() {
        let transaction =
            alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(pb::BtcCoin::Btc)));
        mock_unlocked();
        let tx = transaction.clone();
        static mut COUNTER: u32 = 0;
        *crate::hww::MOCK_NEXT_REQUEST.0.borrow_mut() =
            Some(Box::new(move |_response: Response| {
                unsafe { COUNTER += 1 }
                // The first input is only expected once, the other times other parts of the
                // transaction are expected.
                Ok(Request::BtcSignInput(tx.borrow().inputs[0].input.clone()))
            }));

        let result = block_on(process(&transaction.borrow().init_request()));
        assert_eq!(result, Err(Error::InvalidState));
        assert_eq!(unsafe { COUNTER }, 2);
    }

    /// Test signing if all inputs are of type P2WPKH-P2SH.
    #[test]
    pub fn test_script_type_p2wpkh_p2sh() {
        let transaction =
            alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(pb::BtcCoin::Btc)));
        for input in transaction.borrow_mut().inputs.iter_mut() {
            input.input.keypath[0] = 49 + HARDENED;
        }
        for output in transaction.borrow_mut().outputs.iter_mut() {
            if output.ours {
                output.keypath[0] = 49 + HARDENED;
            }
        }

        mock_host_responder(transaction.clone());
        mock_default_ui();
        mock_unlocked();
        let mut init_request = transaction.borrow().init_request();
        init_request.script_configs[0] = pb::BtcScriptConfigWithKeypath {
            script_config: Some(pb::BtcScriptConfig {
                config: Some(pb::btc_script_config::Config::SimpleType(
                    pb::btc_script_config::SimpleType::P2wpkhP2sh as _,
                )),
            }),
            keypath: vec![49 + HARDENED, 0 + HARDENED, 10 + HARDENED],
        };
        let result = block_on(process(&init_request));
        match result {
            Ok(Response::BtcSignNext(next)) => {
                assert!(next.has_signature);
                assert_eq!(&next.signature, b"\x3a\x46\x18\xf6\x16\x3c\x1d\x55\x3b\xeb\xc2\xc6\xac\x08\x86\x6d\x9f\x02\x7c\xa6\x63\xee\xa7\x43\x65\x8b\xb0\x58\x1c\x42\x33\xa4\x32\x98\x4c\xca\xeb\x52\x04\x4f\x70\x47\x47\x94\xc5\x54\x46\xa5\xd8\x23\xe1\xfb\x96\x9a\x39\x13\x2f\x7d\xa2\x30\xd2\xdd\x33\x75");
            }
            _ => panic!("wrong result"),
        }
    }

    /// Test invalid input cases.
    #[test]
    pub fn test_invalid_input() {
        enum TestCase {
            // all inputs should be the same coin type.
            WrongCoinInput,
            // all change outputs should be the same coin type.
            WrongCoinChange,
            // all inputs should be from the same account.
            WrongAccountInput,
            // all change outputs should go the same account.
            WrongAccountChange,
            // change num in bip44, should be 1.
            WrongBip44Change(u32),
            // referenced script config does not exist.
            InvalidInputScriptConfigIndex,
            // referenced script config does not exist.
            InvalidChangeScriptConfigIndex,
            // sequence number below 0xffffffff - 2
            WrongSequenceNumber,
            // value 0 is invalid
            WrongOutputValue,
            // input value does not match prevtx output value
            WrongInputValue,
            // input's prevtx hash does not match input's prevOutHash
            WrongPrevoutHash,
            // no inputs in prevtx
            PrevTxNoInputs,
            // no outputs in prevtx
            PrevTxNoOutputs,
        }
        for value in [
            TestCase::WrongCoinInput,
            TestCase::WrongCoinChange,
            TestCase::WrongAccountInput,
            TestCase::WrongAccountChange,
            TestCase::WrongBip44Change(0),
            TestCase::WrongBip44Change(2),
            TestCase::InvalidInputScriptConfigIndex,
            TestCase::InvalidChangeScriptConfigIndex,
            TestCase::WrongSequenceNumber,
            TestCase::WrongOutputValue,
            TestCase::WrongInputValue,
            TestCase::WrongPrevoutHash,
            TestCase::PrevTxNoInputs,
            TestCase::PrevTxNoOutputs,
        ] {
            let transaction =
                alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(pb::BtcCoin::Btc)));
            match value {
                TestCase::WrongCoinInput => {
                    transaction.borrow_mut().inputs[0].input.keypath[1] = 1 + HARDENED;
                }
                TestCase::WrongCoinChange => {
                    transaction.borrow_mut().outputs[4].keypath[1] = 1 + HARDENED;
                }
                TestCase::WrongAccountInput => {
                    transaction.borrow_mut().inputs[0].input.keypath[2] += 1;
                }
                TestCase::WrongAccountChange => {
                    transaction.borrow_mut().outputs[4].keypath[2] += 1;
                }
                TestCase::WrongBip44Change(change) => {
                    transaction.borrow_mut().outputs[4].keypath[3] = change;
                }
                TestCase::InvalidInputScriptConfigIndex => {
                    transaction.borrow_mut().inputs[0].input.script_config_index = 1;
                }
                TestCase::InvalidChangeScriptConfigIndex => {
                    transaction.borrow_mut().outputs[4].script_config_index = 1;
                }
                TestCase::WrongSequenceNumber => {
                    transaction.borrow_mut().inputs[0].input.sequence = 0;
                }
                TestCase::WrongOutputValue => {
                    transaction.borrow_mut().outputs[0].value = 0;
                }
                TestCase::WrongInputValue => {
                    transaction.borrow_mut().inputs[0].input.prev_out_value += 1;
                }
                TestCase::WrongPrevoutHash => {
                    transaction.borrow_mut().inputs[0].input.prev_out_hash[0] += 1;
                }
                TestCase::PrevTxNoInputs => {
                    transaction.borrow_mut().inputs[0].prevtx_inputs.clear();
                }
                TestCase::PrevTxNoOutputs => {
                    transaction.borrow_mut().inputs[0].prevtx_outputs.clear();
                }
            }
            mock_host_responder(transaction.clone());
            mock_default_ui();
            mock_unlocked();
            let result = block_on(process(&transaction.borrow().init_request()));
            assert_eq!(result, Err(Error::InvalidInput));
        }
    }

    /// Test signing with mixed input types.
    #[test]
    pub fn test_mixed_inputs() {
        let transaction =
            alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(pb::BtcCoin::Btc)));
        transaction.borrow_mut().inputs[0].input.script_config_index = 1;
        transaction.borrow_mut().inputs[0].input.keypath[0] = 49 + HARDENED;
        mock_host_responder(transaction.clone());
        mock_default_ui();
        mock_unlocked();
        let mut init_request = transaction.borrow().init_request();
        init_request
            .script_configs
            .push(pb::BtcScriptConfigWithKeypath {
                script_config: Some(pb::BtcScriptConfig {
                    config: Some(pb::btc_script_config::Config::SimpleType(
                        pb::btc_script_config::SimpleType::P2wpkhP2sh as _,
                    )),
                }),
                keypath: vec![49 + HARDENED, 0 + HARDENED, 10 + HARDENED],
            });
        assert!(block_on(process(&init_request)).is_ok());
    }

    #[test]
    fn test_user_aborts() {
        let transaction =
            alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(pb::BtcCoin::Btc)));
        mock_host_responder(transaction.clone());
        static mut UI_COUNTER: u32 = 0;
        static mut CURRENT_COUNTER: u32 = 0;
        // We go through all possible user confirmations and abort one of them at a time.
        for counter in 1..=transaction.borrow().total_confirmations {
            unsafe {
                UI_COUNTER = 0;
                CURRENT_COUNTER = counter
            }
            bitbox02::app_btc_sign_ui::mock(bitbox02::app_btc_sign_ui::Ui {
                verify_recipient: Box::new(|_recipient, _amount| unsafe {
                    UI_COUNTER += 1;
                    UI_COUNTER != CURRENT_COUNTER
                }),
                confirm: Box::new(|_title, _body| unsafe {
                    UI_COUNTER += 1;
                    UI_COUNTER != CURRENT_COUNTER
                }),
                verify_total: Box::new(|_total, _fee| unsafe {
                    UI_COUNTER += 1;
                    UI_COUNTER != CURRENT_COUNTER
                }),
            });
            mock_unlocked();
            assert_eq!(
                block_on(process(&transaction.borrow().init_request())),
                Err(Error::UserAbort)
            );
        }
    }

    /// Check workflow when a locktime applies.
    #[test]
    fn test_locktime() {
        struct Test {
            coin: pb::BtcCoin,
            locktime: u32,
            sequence: u32,
            // If None: no user confirmation expected.
            // If Some: confirmation body and user response.
            confirm: Option<(&'static str, bool)>,
        }
        static mut LOCKTIME_CONFIRMED: bool = false;
        for test_case in &[
            Test {
                coin: pb::BtcCoin::Btc,
                locktime: 0,
                sequence: 0xffffffff,
                confirm: None,
            },
            Test {
                coin: pb::BtcCoin::Btc,
                locktime: 0,
                sequence: 0xffffffff - 1,
                confirm: None,
            },
            Test {
                coin: pb::BtcCoin::Btc,
                locktime: 0,
                sequence: 0xffffffff - 2,
                confirm: None,
            },
            Test {
                coin: pb::BtcCoin::Btc,
                locktime: 1,
                sequence: 0xffffffff - 1,
                confirm: Some(("Locktime on block:\n1\nTransaction is not RBF", true)),
            },
            Test {
                coin: pb::BtcCoin::Btc,
                locktime: 1,
                sequence: 0xffffffff - 1,
                confirm: Some(("Locktime on block:\n1\nTransaction is not RBF", false)),
            },
            Test {
                coin: pb::BtcCoin::Btc,
                locktime: 10,
                sequence: 0xffffffff - 1,
                confirm: Some(("Locktime on block:\n10\nTransaction is not RBF", true)),
            },
            Test {
                coin: pb::BtcCoin::Btc,
                locktime: 10,
                sequence: 0xffffffff - 2,
                confirm: Some(("Locktime on block:\n10\nTransaction is RBF", true)),
            },
            Test {
                coin: pb::BtcCoin::Ltc,
                locktime: 10,
                sequence: 0xffffffff - 1,
                confirm: Some(("Locktime on block:\n10\n", true)),
            },
            Test {
                coin: pb::BtcCoin::Ltc,
                locktime: 10,
                sequence: 0xffffffff - 2,
                confirm: Some(("Locktime on block:\n10\n", true)),
            },
        ] {
            let transaction =
                alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(test_case.coin)));
            transaction.borrow_mut().inputs[0].input.sequence = test_case.sequence;
            mock_host_responder(transaction.clone());
            unsafe { LOCKTIME_CONFIRMED = false }
            bitbox02::app_btc_sign_ui::mock(bitbox02::app_btc_sign_ui::Ui {
                verify_recipient: Box::new(|_recipient, _amount| true),
                confirm: Box::new(move |title, body| {
                    if body.contains("Locktime") {
                        if let Some((confirm_str, user_response)) = test_case.confirm {
                            assert_eq!(title, "");
                            assert_eq!(body, confirm_str);
                            unsafe { LOCKTIME_CONFIRMED = true }
                            return user_response;
                        }
                        panic!("Unexpected RBF confirmation");
                    }
                    true
                }),
                verify_total: Box::new(|_total, _fee| true),
            });

            mock_unlocked();

            let mut init_request = transaction.borrow().init_request();
            init_request.locktime = test_case.locktime;
            let result = block_on(process(&init_request));
            if let Some((_, false)) = test_case.confirm {
                assert_eq!(result, Err(Error::UserAbort));
            } else {
                assert!(result.is_ok());
            }
            assert_eq!(unsafe { LOCKTIME_CONFIRMED }, test_case.confirm.is_some());
        }
    }

    // Test a P2TR output. It is not part of the default test transaction because Taproot is not
    // active on Litecoin yet.
    #[test]
    fn test_p2tr_output() {
        let transaction =
            alloc::rc::Rc::new(core::cell::RefCell::new(Transaction::new(pb::BtcCoin::Btc)));
        transaction.borrow_mut().outputs[0].r#type = pb::BtcOutputType::P2tr as _;
        transaction.borrow_mut().outputs[0].payload = b"\xa6\x08\x69\xf0\xdb\xcf\x1d\xc6\x59\xc9\xce\xcb\xaf\x80\x50\x13\x5e\xa9\xe8\xcd\xc4\x87\x05\x3f\x1d\xc6\x88\x09\x49\xdc\x68\x4c".to_vec();
        mock_host_responder(transaction.clone());
        static mut UI_COUNTER: u32 = 0;
        bitbox02::app_btc_sign_ui::mock(bitbox02::app_btc_sign_ui::Ui {
            verify_recipient: Box::new(|recipient, amount| unsafe {
                UI_COUNTER += 1;
                if UI_COUNTER == 1 {
                    assert_eq!(
                        recipient,
                        "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr"
                    );
                    assert_eq!(amount, "1 BTC");
                }
                true
            }),
            confirm: Box::new(|_title, _body| true),
            verify_total: Box::new(|_total, _fee| true),
        });
        mock_unlocked();
        let result = block_on(process(&transaction.borrow().init_request()));
        assert!(unsafe { UI_COUNTER >= 1 });
        match result {
            Ok(Response::BtcSignNext(next)) => {
                assert!(next.has_signature);
                assert_eq!(&next.signature, b"\x8f\x1e\x0e\x8f\x98\xd3\x6d\xb1\x19\x62\x64\xf1\xa3\x00\xfa\xe3\x17\xf1\x50\x8d\x2c\x48\x9f\xbb\xd6\x60\xe0\x48\xc4\x52\x9c\x61\x2f\x59\x57\x6c\x86\xa2\x6f\xfa\x47\x6d\x97\x35\x1e\x46\x9e\xf6\xed\x27\x84\xae\xcb\x71\x05\x3a\x51\x66\x77\x5c\xcb\x4d\x7b\x9b");
            }
            _ => panic!("wrong result"),
        }
    }
}
