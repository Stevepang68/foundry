pub use crate::ic::*;
use crate::{
    backend::DatabaseExt, constants::DEFAULT_CREATE2_DEPLOYER_CODEHASH, precompiles::ODYSSEY_P256,
    Env, InspectorExt,
};
use alloy_consensus::BlockHeader;
use alloy_evm::eth::EthEvmContext;
use alloy_json_abi::{Function, JsonAbi};
use alloy_network::{AnyTxEnvelope, TransactionResponse};
use alloy_primitives::{Address, Bytes, Selector, TxKind, B256, U256};
use alloy_provider::{network::BlockResponse, Network};
use alloy_rpc_types::{Transaction, TransactionRequest};
use foundry_common::is_impersonated_tx;
use foundry_config::NamedChain;
use foundry_fork_db::DatabaseError;
use revm::{
    context::{ContextTr, Evm, EvmData, JournalInner},
    context_interface::{result::EVMError, CreateScheme},
    handler::{
        instructions::{EthInstructions, InstructionProvider},
        EthPrecompiles, EvmTr, FrameOrResult, FrameResult, Handler, PrecompileProvider,
    },
    inspector::{inspect_instructions, InspectorEvmTr},
    interpreter::{
        interpreter::EthInterpreter, return_ok, CallInputs, CallOutcome, CallScheme, CallValue,
        CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter, InterpreterResult,
        InterpreterTypes,
    },
    precompile::PrecompileError,
    primitives::{hardfork::SpecId, KECCAK_EMPTY},
    Journal,
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

pub use revm::state::EvmState as StateChangeset;

/// Depending on the configured chain id and block number this should apply any specific changes
///
/// - checks for prevrandao mixhash after merge
/// - applies chain specifics: on Arbitrum `block.number` is the L1 block
///
/// Should be called with proper chain id (retrieved from provider if not provided).
pub fn apply_chain_and_block_specific_env_changes<N: Network>(
    env: &mut crate::Env,
    block: &N::BlockResponse,
) {
    use NamedChain::*;
    if let Ok(chain) = NamedChain::try_from(env.evm_env.cfg_env.chain_id) {
        let block_number = block.header().number();

        match chain {
            Mainnet => {
                // after merge difficulty is supplanted with prevrandao EIP-4399
                if block_number >= 15_537_351u64 {
                    env.evm_env.block_env.difficulty =
                        env.evm_env.block_env.prevrandao.unwrap_or_default().into();
                }

                return;
            }
            BinanceSmartChain | BinanceSmartChainTestnet => {
                // https://github.com/foundry-rs/foundry/issues/9942
                // As far as observed from the source code of bnb-chain/bsc, the `difficulty` field
                // is still in use and returned by the corresponding opcode but `prevrandao`
                // (`mixHash`) is always zero, even though bsc adopts the newer EVM
                // specification. This will confuse revm and causes emulation
                // failure.
                env.evm_env.block_env.prevrandao = Some(env.evm_env.block_env.difficulty.into());
                return;
            }
            Moonbeam | Moonbase | Moonriver | MoonbeamDev => {
                if env.evm_env.block_env.prevrandao.is_none() {
                    // <https://github.com/foundry-rs/foundry/issues/4232>
                    env.evm_env.block_env.prevrandao = Some(B256::random());
                }
            }
            c if c.is_arbitrum() => {
                // on arbitrum `block.number` is the L1 block which is included in the
                // `l1BlockNumber` field
                if let Some(l1_block_number) = block
                    .other_fields()
                    .and_then(|other| other.get("l1BlockNumber").cloned())
                    .and_then(|v| v.as_u64())
                {
                    env.evm_env.block_env.number = l1_block_number;
                }
            }
            _ => {}
        }
    }

    // if difficulty is `0` we assume it's past merge
    if block.header().difficulty().is_zero() {
        env.evm_env.block_env.difficulty =
            env.evm_env.block_env.prevrandao.unwrap_or_default().into();
    }
}

/// Given an ABI and selector, it tries to find the respective function.
pub fn get_function<'a>(
    contract_name: &str,
    selector: Selector,
    abi: &'a JsonAbi,
) -> eyre::Result<&'a Function> {
    abi.functions()
        .find(|func| func.selector() == selector)
        .ok_or_else(|| eyre::eyre!("{contract_name} does not have the selector {selector}"))
}

/// Configures the env for the given RPC transaction.
/// Accounts for an impersonated transaction by resetting the `env.tx.caller` field to `tx.from`.
pub fn configure_tx_env(env: &mut crate::Env, tx: &Transaction<AnyTxEnvelope>) {
    let impersonated_from = is_impersonated_tx(&tx.inner).then_some(tx.from());
    if let AnyTxEnvelope::Ethereum(tx) = &tx.inner.inner() {
        configure_tx_req_env(env, &tx.clone().into(), impersonated_from).expect("cannot fail");
    }
}

/// Configures the env for the given RPC transaction request.
/// `impersonated_from` is the address of the impersonated account. This helps account for an
/// impersonated transaction by resetting the `env.tx.caller` field to `impersonated_from`.
pub fn configure_tx_req_env(
    env: &mut crate::Env,
    tx: &TransactionRequest,
    impersonated_from: Option<Address>,
) -> eyre::Result<()> {
    let TransactionRequest {
        nonce,
        from,
        to,
        value,
        gas_price,
        gas,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        max_fee_per_blob_gas,
        ref input,
        chain_id,
        ref blob_versioned_hashes,
        ref access_list,
        transaction_type: _,
        ref authorization_list,
        sidecar: _,
    } = *tx;

    // If no `to` field then set create kind: https://eips.ethereum.org/EIPS/eip-2470#deployment-transaction
    env.tx.kind = to.unwrap_or(TxKind::Create);
    // If the transaction is impersonated, we need to set the caller to the from
    // address Ref: https://github.com/foundry-rs/foundry/issues/9541
    env.tx.caller =
        impersonated_from.unwrap_or(from.ok_or_else(|| eyre::eyre!("missing `from` field"))?);
    env.tx.gas_limit = gas.ok_or_else(|| eyre::eyre!("missing `gas` field"))?;
    env.tx.nonce = nonce.unwrap_or_default();
    env.tx.value = value.unwrap_or_default();
    env.tx.data = input.input().cloned().unwrap_or_default();
    env.tx.chain_id = chain_id;

    // Type 1, EIP-2930
    env.tx.access_list = access_list.clone().unwrap_or_default();

    // Type 2, EIP-1559
    env.tx.gas_price = gas_price.or(max_fee_per_gas).unwrap_or_default();
    env.tx.gas_priority_fee = max_priority_fee_per_gas;

    // Type 3, EIP-4844
    env.tx.blob_hashes = blob_versioned_hashes.clone().unwrap_or_default();
    env.tx.max_fee_per_blob_gas = max_fee_per_blob_gas.unwrap_or_default();

    // Type 4, EIP-7702
    if let Some(authorization_list) = authorization_list {
        env.tx.authorization_list = authorization_list.clone();
    }

    Ok(())
}

/// Get the gas used, accounting for refunds
pub fn gas_used(spec: SpecId, spent: u64, refunded: u64) -> u64 {
    let refund_quotient = if SpecId::is_enabled_in(spec, SpecId::LONDON) { 5 } else { 2 };
    spent - (refunded).min(spent / refund_quotient)
}

fn get_create2_factory_call_inputs(
    salt: U256,
    inputs: CreateInputs,
    deployer: Address,
) -> CallInputs {
    let calldata = [&salt.to_be_bytes::<32>()[..], &inputs.init_code[..]].concat();
    CallInputs {
        caller: inputs.caller,
        bytecode_address: deployer,
        target_address: deployer,
        scheme: CallScheme::Call,
        value: CallValue::Transfer(inputs.value),
        input: calldata.into(),
        gas_limit: inputs.gas_limit,
        is_static: false,
        return_memory_offset: 0..0,
        is_eof: false,
    }
}
