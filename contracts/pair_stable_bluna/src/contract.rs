use crate::error::ContractError;
use crate::math::{
    calc_ask_amount, calc_offer_amount, compute_d, AMP_PRECISION, MAX_AMP, MAX_AMP_CHANGE,
    MIN_AMP_CHANGING_TIME, N_COINS,
};
use crate::state::{
    Config, BLUNA_REWARD_GLOBAL_INDEX, BLUNA_REWARD_HOLDER, BLUNA_REWARD_USER_INDEXES, CONFIG,
};

use cosmwasm_bignumber::Decimal256;
use cosmwasm_std::{
    attr, entry_point, from_binary, to_binary, Addr, Binary, Coin, CosmosMsg, Decimal, Deps,
    DepsMut, Env, MessageInfo, Reply, ReplyOn, Response, StdError, StdResult, SubMsg, Uint128,
    Uint256, WasmMsg,
};

use crate::response::MsgInstantiateContractResponse;
use astroport::asset::{addr_validate_to_lower, format_lp_token_name, Asset, AssetInfo, PairInfo};
use astroport::factory::PairType;

use astroport::generator::{
    Cw20HookMsg as GeneratorHookMsg, PoolInfoResponse, QueryMsg as GeneratorQueryMsg,
};
use astroport::pair::{
    ConfigResponse, CumulativePricesResponse, Cw20HookMsg, InstantiateMsg, PoolResponse,
    ReverseSimulationResponse, SimulationResponse, DEFAULT_SLIPPAGE, MAX_ALLOWED_SLIPPAGE,
    TWAP_PRECISION,
};
use astroport::pair_stable_bluna::{
    ExecuteMsg, MigrateMsg, QueryMsg, StablePoolConfig, StablePoolParams, StablePoolUpdateParams,
};
use astroport::whitelist::InstantiateMsg as WhitelistInstantiateMsg;

use astroport::querier::{
    query_factory_config, query_fee_info, query_supply, query_token_precision,
};
use astroport::{token::InstantiateMsg as TokenInstantiateMsg, U256};
use cw2::{get_contract_version, set_contract_version};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg, MinterResponse};
use protobuf::Message;
use std::cmp::Ordering;
use std::convert::TryInto;
use std::str::FromStr;
use std::vec;

/// Contract name that is used for migration.
const CONTRACT_NAME: &str = "astroport-pair-stable-bluna";
/// Contract version that is used for migration.
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// `reply` call code IDs used for sub-messages.
const INSTANTIATE_TOKEN_REPLY_ID: u64 = 1;
const INSTANTIATE_BLUNA_REWARD_HOLDER_REPLY_ID: u64 = 2;

/// ## Description
/// Creates a new contract with the specified parameters in the [`InstantiateMsg`].
/// Returns a [`Response`] with the specified attributes if the operation was successful,
/// or a [`ContractError`] if the contract was not created.
/// ## Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **_info** is an object of type [`MessageInfo`].
/// * **msg** is a message of type [`InstantiateMsg`] which contains the parameters used for creating the contract.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    msg.asset_infos[0].check(deps.api)?;
    msg.asset_infos[1].check(deps.api)?;

    if msg.asset_infos[0] == msg.asset_infos[1] {
        return Err(ContractError::DoublingAssets {});
    }

    if msg.init_params.is_none() {
        return Err(ContractError::InitParamsNotFound {});
    }

    let params: StablePoolParams = from_binary(&msg.init_params.unwrap())?;

    if params.amp == 0 || params.amp > MAX_AMP {
        return Err(ContractError::IncorrectAmp {});
    }

    let mut messages: Vec<SubMsg> = vec![get_bluna_reward_holder_instantiating_message(
        deps.as_ref(),
        &env,
        &addr_validate_to_lower(deps.api, &msg.factory_addr)?,
    )?];

    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let config = Config {
        pair_info: PairInfo {
            contract_addr: env.contract.address.clone(),
            liquidity_token: Addr::unchecked(""),
            asset_infos: msg.asset_infos.clone(),
            pair_type: PairType::Stable {},
        },
        factory_addr: addr_validate_to_lower(deps.api, msg.factory_addr.as_str())?,
        block_time_last: 0,
        price0_cumulative_last: Uint128::zero(),
        price1_cumulative_last: Uint128::zero(),
        init_amp: params.amp * AMP_PRECISION,
        init_amp_time: env.block.time.seconds(),
        next_amp: params.amp * AMP_PRECISION,
        next_amp_time: env.block.time.seconds(),
        bluna_rewarder: addr_validate_to_lower(deps.api, params.bluna_rewarder.as_str())?,
        generator: addr_validate_to_lower(deps.api, params.generator.as_str())?,
    };

    CONFIG.save(deps.storage, &config)?;

    let token_name = format_lp_token_name(msg.asset_infos, &deps.querier)?;

    // Create LP token
    messages.push(SubMsg {
        msg: WasmMsg::Instantiate {
            code_id: msg.token_code_id,
            msg: to_binary(&TokenInstantiateMsg {
                name: token_name,
                symbol: "uLP".to_string(),
                decimals: 6,
                initial_balances: vec![],
                mint: Some(MinterResponse {
                    minter: env.contract.address.to_string(),
                    cap: None,
                }),
            })?,
            funds: vec![],
            admin: None,
            label: String::from("Astroport LP token"),
        }
        .into(),
        id: INSTANTIATE_TOKEN_REPLY_ID,
        gas_limit: None,
        reply_on: ReplyOn::Success,
    });

    Ok(Response::new().add_submessages(messages))
}

/// ## Description
/// The entry point to the contract for processing replies from submessages.
/// # Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **_env** is an object of type [`Env`].
///
/// * **msg** is an object of type [`Reply`].
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, _env: Env, msg: Reply) -> Result<Response, ContractError> {
    let data = msg.result.unwrap().data.unwrap();
    let res: MsgInstantiateContractResponse =
        Message::parse_from_bytes(data.as_slice()).map_err(|_| {
            StdError::parse_err("MsgInstantiateContractResponse", "failed to parse data")
        })?;

    let mut response = Response::new();

    match msg.id {
        INSTANTIATE_TOKEN_REPLY_ID => {
            let mut config: Config = CONFIG.load(deps.storage)?;

            if config.pair_info.liquidity_token != Addr::unchecked("") {
                return Err(ContractError::Unauthorized {});
            }
            config.pair_info.liquidity_token =
                addr_validate_to_lower(deps.api, res.get_contract_address())?;

            CONFIG.save(deps.storage, &config)?;

            response.attributes.push(attr(
                "liquidity_token_addr",
                config.pair_info.liquidity_token,
            ));
        }
        INSTANTIATE_BLUNA_REWARD_HOLDER_REPLY_ID => {
            let addr = addr_validate_to_lower(deps.api, res.get_contract_address())?;
            BLUNA_REWARD_HOLDER.save(deps.storage, &addr)?;
            response.attributes.push(attr("bluna_reward_holder", addr))
        }
        _ => return Err(ContractError::Unauthorized {}),
    };

    Ok(response)
}

/// ## Description
/// Exposes all the execute functions available in the contract.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **msg** is an object of type [`ExecuteMsg`].
///
/// ## Queries
/// * **ExecuteMsg::UpdateConfig { params: Binary }** Not supported.
///
/// * **ExecuteMsg::Receive(msg)** Receives a message of type [`Cw20ReceiveMsg`] and processes
/// it depending on the received template.
///
/// * **ExecuteMsg::ProvideLiquidity {
///             assets,
///             slippage_tolerance,
///             auto_stake,
///             receiver,
///         }** Provides liquidity in the pair with the specified input parameters.
///
/// * **ExecuteMsg::Swap {
///             offer_asset,
///             belief_price,
///             max_spread,
///             to,
///         }** Performs a swap operation with the specified parameters.
///
/// * **ExecuteMsg::ClaimReward {
///             receiver,
///             user_share,
///             total_share,
///         }** Claims bLUNA rewards and sends them to the receiver.
///
/// * **ExecuteMsg::ClaimRewardByGenerator {
///             receiver,
///             user_share,
///             total_share,
///         }** Claims bLUNA rewards for a LP position that was staked by a user in the Astroprot Generator contract.
///
/// * **ExecuteMsg::HandleReward {
///             previous_reward_balance,
///             user_share,
///             total_share,
///             user,
///         }** Handles and distributes bLUNA rewards.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::UpdateConfig { params } => update_config(deps, env, info, params),
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::ProvideLiquidity {
            assets,
            slippage_tolerance,
            auto_stake,
            receiver,
        } => provide_liquidity(
            deps,
            env,
            info,
            assets,
            slippage_tolerance,
            auto_stake,
            receiver,
        ),
        ExecuteMsg::Swap {
            offer_asset,
            belief_price,
            max_spread,
            to,
        } => {
            offer_asset.info.check(deps.api)?;
            if !offer_asset.is_native_token() {
                return Err(ContractError::Unauthorized {});
            }

            let to_addr = if let Some(to_addr) = to {
                Some(addr_validate_to_lower(deps.api, &to_addr)?)
            } else {
                None
            };

            swap(
                deps,
                env,
                info.clone(),
                info.sender,
                offer_asset,
                belief_price,
                max_spread,
                to_addr,
            )
        }
        ExecuteMsg::ClaimReward { receiver } => claim_reward(deps, env, info, receiver),
        ExecuteMsg::ClaimRewardByGenerator {
            user,
            user_share,
            total_share,
        } => claim_reward_by_generator(deps, env, info, user, user_share, total_share),
        ExecuteMsg::HandleReward {
            previous_reward_balance,
            user,
            user_share,
            total_share,
            receiver,
        } => handle_reward(
            deps,
            env,
            info,
            previous_reward_balance,
            user,
            user_share,
            total_share,
            receiver,
        ),
    }
}

/// ## Description
/// Receives a message of type [`Cw20ReceiveMsg`] and processes it depending on the received template.
/// If the template is not found in the received message, then a [`ContractError`] is returned,
/// otherwise it returns a [`Response`] with the specified attributes if the operation was successful.
/// ## Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **cw20_msg** is an object of type [`Cw20ReceiveMsg`]. This is the CW20 message that has to be processed.
pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let contract_addr = info.sender.clone();
    match from_binary(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Swap {
            belief_price,
            max_spread,
            to,
        }) => {
            // Only an asset contract can execute this message
            let mut authorized: bool = false;
            let config: Config = CONFIG.load(deps.storage)?;

            for pool in config.pair_info.asset_infos {
                if let AssetInfo::Token { contract_addr, .. } = &pool {
                    if contract_addr == &info.sender {
                        authorized = true;
                    }
                }
            }

            if !authorized {
                return Err(ContractError::Unauthorized {});
            }

            let to_addr = if let Some(to_addr) = to {
                Some(addr_validate_to_lower(deps.api, to_addr.as_str())?)
            } else {
                None
            };

            swap(
                deps,
                env,
                info,
                Addr::unchecked(cw20_msg.sender),
                Asset {
                    info: AssetInfo::Token { contract_addr },
                    amount: cw20_msg.amount,
                },
                belief_price,
                max_spread,
                to_addr,
            )
        }
        Ok(Cw20HookMsg::WithdrawLiquidity {}) => withdraw_liquidity(
            deps,
            env,
            info,
            Addr::unchecked(cw20_msg.sender),
            cw20_msg.amount,
        ),
        Err(err) => Err(ContractError::Std(err)),
    }
}

/// ## Description
/// Provides liquidity with the specified input parameters.
/// Returns a [`ContractError`] on failure, otherwise returns a [`Response`] with the
/// specified attributes if the operation was successful.
/// ## Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **assets** is an array with two objects of type [`Asset`]. These are the assets available in the pool.
///
/// * **slippage_tolerance** is an object of type [`Option<Decimal>`]. This is the slippage tolerance for providing liquidity.
///
/// * **auto_stake** is an object of type [`Option<bool>`]. Determines whether the resulting LP tokens are automatically staked in the Generator contract to receive token incentives.
///
/// * **receiver** is an object of type [`Option<String>`]. This is the address that receives LP tokens.
/// If this address isn't specified, the function will default to the caller.
/// NOTE - the address that wants to swap should approve the pair contract to pull the offer token.
pub fn provide_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    assets: [Asset; 2],
    slippage_tolerance: Option<Decimal>,
    auto_stake: Option<bool>,
    receiver: Option<String>,
) -> Result<Response, ContractError> {
    assets[0].info.check(deps.api)?;
    assets[1].info.check(deps.api)?;

    let auto_stake = auto_stake.unwrap_or(false);
    for asset in assets.iter() {
        asset.assert_sent_native_token_balance(&info)?;
    }

    let mut config: Config = CONFIG.load(deps.storage)?;
    let mut pools: [Asset; 2] = config
        .pair_info
        .query_pools(&deps.querier, env.contract.address.clone())?;
    let deposits: [Uint128; 2] = [
        assets
            .iter()
            .find(|a| a.info.equal(&pools[0].info))
            .map(|a| a.amount)
            .expect("Wrong asset info is given"),
        assets
            .iter()
            .find(|a| a.info.equal(&pools[1].info))
            .map(|a| a.amount)
            .expect("Wrong asset info is given"),
    ];

    if deposits[0].is_zero() || deposits[1].is_zero() {
        return Err(ContractError::InvalidZeroAmount {});
    }

    let mut messages: Vec<CosmosMsg> = vec![];
    for (i, pool) in pools.iter_mut().enumerate() {
        // If the pool is a token contract, then we need to execute a TransferFrom msg to receive funds
        if let AssetInfo::Token { contract_addr, .. } = &pool.info {
            messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.to_string(),
                msg: to_binary(&Cw20ExecuteMsg::TransferFrom {
                    owner: info.sender.to_string(),
                    recipient: env.contract.address.to_string(),
                    amount: deposits[i],
                })?,
                funds: vec![],
            }))
        } else {
            // If the asset is a native token, the pool balance already increased
            // To calculate the pool balance properly, we should subtract the user deposit from the recorded pool token amount
            pool.amount = pool.amount.checked_sub(deposits[i])?;
        }
    }

    // Assert slippage tolerance
    assert_slippage_tolerance(&slippage_tolerance, &deposits, &pools)?;

    let token_precision_0 = query_token_precision(&deps.querier, pools[0].info.clone())?;
    let token_precision_1 = query_token_precision(&deps.querier, pools[1].info.clone())?;

    let greater_precision = token_precision_0.max(token_precision_1);

    let deposit_amount_0 = adjust_precision(deposits[0], token_precision_0, greater_precision)?;
    let deposit_amount_1 = adjust_precision(deposits[1], token_precision_1, greater_precision)?;

    let total_share = query_supply(&deps.querier, config.pair_info.liquidity_token.clone())?;
    let share = if total_share.is_zero() {
        let liquidity_token_precision = query_token_precision(
            &deps.querier,
            AssetInfo::Token {
                contract_addr: config.pair_info.liquidity_token.clone(),
            },
        )?;

        // Initial share = collateral amount
        adjust_precision(
            Uint128::new(
                (U256::from(deposit_amount_0.u128()) * U256::from(deposit_amount_1.u128()))
                    .integer_sqrt()
                    .as_u128(),
            ),
            greater_precision,
            liquidity_token_precision,
        )?
    } else {
        let leverage = compute_current_amp(&config, &env)?
            .checked_mul(u64::from(N_COINS))
            .unwrap();

        let mut pool_amount_0 =
            adjust_precision(pools[0].amount, token_precision_0, greater_precision)?;
        let mut pool_amount_1 =
            adjust_precision(pools[1].amount, token_precision_1, greater_precision)?;

        let d_before_addition_liquidity =
            compute_d(leverage, pool_amount_0.u128(), pool_amount_1.u128()).unwrap();

        pool_amount_0 = pool_amount_0.checked_add(deposit_amount_0)?;
        pool_amount_1 = pool_amount_1.checked_add(deposit_amount_1)?;

        let d_after_addition_liquidity =
            compute_d(leverage, pool_amount_0.u128(), pool_amount_1.u128()).unwrap();

        // d after adding liquidity may be less than or equal to d before adding liquidity because of rounding
        if d_before_addition_liquidity >= d_after_addition_liquidity {
            return Err(ContractError::LiquidityAmountTooSmall {});
        }

        total_share.multiply_ratio(
            d_after_addition_liquidity - d_before_addition_liquidity,
            d_before_addition_liquidity,
        )
    };

    if share.is_zero() {
        return Err(ContractError::LiquidityAmountTooSmall {});
    }

    // Mint LP token for the caller (or for the receiver if it was set)
    let receiver = receiver.unwrap_or_else(|| info.sender.to_string());
    messages.extend(mint_liquidity_token_message(
        deps.as_ref(),
        &config,
        env.clone(),
        addr_validate_to_lower(deps.api, receiver.as_str())?,
        share,
        auto_stake,
    )?);

    // Accumulate prices assets in the pool
    if let Some((price0_cumulative_new, price1_cumulative_new, block_time)) = accumulate_prices(
        env,
        &config,
        pools[0].amount,
        token_precision_0,
        pools[1].amount,
        token_precision_1,
    )? {
        config.price0_cumulative_last = price0_cumulative_new;
        config.price1_cumulative_last = price1_cumulative_new;
        config.block_time_last = block_time;
        CONFIG.save(deps.storage, &config)?;
    }

    Ok(Response::new().add_messages(messages).add_attributes(vec![
        attr("action", "provide_liquidity"),
        attr("sender", info.sender.as_str()),
        attr("receiver", receiver.as_str()),
        attr("assets", format!("{}, {}", assets[0], assets[1])),
        attr("share", share.to_string()),
    ]))
}

/// ## Description
/// Mint LP tokens for a beneficiary and auto deposit them into the Generator contract (if requested).
/// # Params
/// * **deps** is an object of type [`Deps`].
///
/// * **config** is an object of type [`Config`].
///
/// * **env** is an object of type [`Env`].
///
/// * **recipient** is an object of type [`Addr`]. This is the LP token recipient.
///
/// * **amount** is an object of type [`Uint128`]. This is the amount of LP tokens to mint.
///
/// * **auto_stake** is a field of type [`bool`]. Determines whether or not LP tokens will be automatically staked in the Generator contract.
fn mint_liquidity_token_message(
    deps: Deps,
    config: &Config,
    env: Env,
    recipient: Addr,
    amount: Uint128,
    auto_stake: bool,
) -> Result<Vec<CosmosMsg>, ContractError> {
    let lp_token = config.pair_info.liquidity_token.clone();

    // If no auto-stake - just mint LP tokens for the recipient and return
    if !auto_stake {
        return Ok(vec![CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token.to_string(),
            msg: to_binary(&Cw20ExecuteMsg::Mint {
                recipient: recipient.to_string(),
                amount,
            })?,
            funds: vec![],
        })]);
    }

    // Mint for the contract and stake into the Generator
    let generator =
        query_factory_config(&deps.querier, config.clone().factory_addr)?.generator_address;

    if generator.is_none() {
        return Err(ContractError::AutoStakeError {});
    }

    Ok(vec![
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token.to_string(),
            msg: to_binary(&Cw20ExecuteMsg::Mint {
                recipient: env.contract.address.to_string(),
                amount,
            })?,
            funds: vec![],
        }),
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token.to_string(),
            msg: to_binary(&Cw20ExecuteMsg::Send {
                contract: generator.unwrap().to_string(),
                amount,
                msg: to_binary(&GeneratorHookMsg::DepositFor(recipient))?,
            })?,
            funds: vec![],
        }),
    ])
}

/// ## Description
/// Withdraw liquidity from the pool. Returns a [`ContractError`] on failure,
/// otherwise returns a [`Response`] with the specified attributes if the operation was successful.
/// ## Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **sender** is an object of type [`Addr`]. This is the address that will receive the withdrawn liquidity.
///
/// * **amount** is an object of type [`Uint128`]. This is the amount of LP tokens to burn and withdraw liquidity with.
pub fn withdraw_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    sender: Addr,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let mut config: Config = CONFIG.load(deps.storage).unwrap();

    if info.sender != config.pair_info.liquidity_token {
        return Err(ContractError::Unauthorized {});
    }

    let (pools, total_share) = pool_info(deps.as_ref(), config.clone())?;
    let refund_assets = get_share_in_assets(&pools, amount, total_share);

    // Accumulate prices for the assets in the pool
    if let Some((price0_cumulative_new, price1_cumulative_new, block_time)) = accumulate_prices(
        env,
        &config,
        pools[0].amount,
        query_token_precision(&deps.querier, pools[0].info.clone())?,
        pools[1].amount,
        query_token_precision(&deps.querier, pools[1].info.clone())?,
    )? {
        config.price0_cumulative_last = price0_cumulative_new;
        config.price1_cumulative_last = price1_cumulative_new;
        config.block_time_last = block_time;
        CONFIG.save(deps.storage, &config)?;
    }

    let messages: Vec<CosmosMsg> = vec![
        refund_assets[0]
            .clone()
            .into_msg(&deps.querier, sender.clone())?,
        refund_assets[1]
            .clone()
            .into_msg(&deps.querier, sender.clone())?,
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.pair_info.liquidity_token.to_string(),
            msg: to_binary(&Cw20ExecuteMsg::Burn { amount })?,
            funds: vec![],
        }),
    ];

    let attributes = vec![
        attr("action", "withdraw_liquidity"),
        attr("sender", sender.as_str()),
        attr("withdrawn_share", &amount.to_string()),
        attr(
            "refund_assets",
            format!("{}, {}", refund_assets[0], refund_assets[1]),
        ),
    ];

    Ok(Response::new()
        .add_messages(messages)
        .add_attributes(attributes))
}

/// ## Description
/// Return the amount of tokens that a specific amount of LP tokens would withdraw.
/// ## Params
/// * **pools** is an array of [`Asset`] type items. These are the assets available in the pool.
///
/// * **amount** is an object of type [`Uint128`]. This is the amount of LP tokens to calculate underlying amounts for.
///
/// * **total_share** is an object of type [`Uint128`]. This is the total amount of LP tokens currently issued by the pool.
pub fn get_share_in_assets(
    pools: &[Asset; 2],
    amount: Uint128,
    total_share: Uint128,
) -> [Asset; 2] {
    let mut share_ratio = Decimal::zero();
    if !total_share.is_zero() {
        share_ratio = Decimal::from_ratio(amount, total_share);
    }

    [
        Asset {
            info: pools[0].info.clone(),
            amount: pools[0].amount * share_ratio,
        },
        Asset {
            info: pools[1].info.clone(),
            amount: pools[1].amount * share_ratio,
        },
    ]
}

/// ## Description
/// Performs a swap with the specified parameters.
/// CONTRACT - the user must allow the pool to pull tokens from their account in order to swap them.
/// Returns a [`ContractError`] on failure, otherwise returns a [`Response`] with the
/// specified attributes if the operation was successful.
/// ## Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **sender** is an object of type [`Addr`]. This is the default recipient of the swap operation.
///
/// * **offer_asset** is an object of type [`Asset`]. This is the asset to swap.
///
/// * **belief_price** is an object of type [`Option<Decimal>`]. This is used to calculate the maximum spread.
///
/// * **max_spread** is an object of type [`Option<Decimal>`]. This is the maximum spread allowed for the swap.
///
/// * **to** is an object of type [`Option<Addr>`]. This is the address that receives ask tokens.
#[allow(clippy::too_many_arguments)]
pub fn swap(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    sender: Addr,
    offer_asset: Asset,
    belief_price: Option<Decimal>,
    max_spread: Option<Decimal>,
    to: Option<Addr>,
) -> Result<Response, ContractError> {
    offer_asset.assert_sent_native_token_balance(&info)?;

    let mut config: Config = CONFIG.load(deps.storage)?;

    // If the asset balance already increased
    // We should subtract the user deposit from the pool offer asset amount
    let pools: Vec<Asset> = config
        .pair_info
        .query_pools(&deps.querier, env.clone().contract.address)?
        .iter()
        .map(|p| {
            let mut p = p.clone();
            if p.info.equal(&offer_asset.info) {
                p.amount = p.amount.checked_sub(offer_asset.amount).unwrap();
            }

            p
        })
        .collect();

    let offer_pool: Asset;
    let ask_pool: Asset;

    if offer_asset.info.equal(&pools[0].info) {
        offer_pool = pools[0].clone();
        ask_pool = pools[1].clone();
    } else if offer_asset.info.equal(&pools[1].info) {
        offer_pool = pools[1].clone();
        ask_pool = pools[0].clone();
    } else {
        return Err(ContractError::AssetMismatch {});
    }

    // Get fee info from the factory
    let fee_info = query_fee_info(
        &deps.querier,
        config.factory_addr.clone(),
        config.pair_info.pair_type.clone(),
    )?;

    let offer_amount = offer_asset.amount;
    let (return_amount, spread_amount, commission_amount) = compute_swap(
        offer_pool.amount,
        query_token_precision(&deps.querier, offer_pool.info)?,
        ask_pool.amount,
        query_token_precision(&deps.querier, ask_pool.info.clone())?,
        offer_amount,
        fee_info.total_fee_rate,
        compute_current_amp(&config, &env)?,
    )?;

    // Check the max spread limit (if it was specified)
    assert_max_spread(
        belief_price,
        max_spread,
        offer_amount,
        return_amount + commission_amount,
        spread_amount,
    )?;

    // Compute the tax for the ask asset
    let return_asset = Asset {
        info: ask_pool.info.clone(),
        amount: return_amount,
    };

    let tax_amount = return_asset.compute_tax(&deps.querier)?;

    let receiver = to.unwrap_or_else(|| sender.clone());

    let mut messages: Vec<CosmosMsg> =
        vec![return_asset.into_msg(&deps.querier, receiver.clone())?];

    // Maker fee
    let mut maker_fee_amount = Uint128::new(0);
    if let Some(fee_address) = fee_info.fee_address {
        if let Some(f) = calculate_maker_fee(
            ask_pool.info.clone(),
            commission_amount,
            fee_info.maker_fee_rate,
        ) {
            messages.push(f.clone().into_msg(&deps.querier, fee_address)?);
            maker_fee_amount = f.amount;
        }
    }

    // Accumulate prices for the assets in the pool
    if let Some((price0_cumulative_new, price1_cumulative_new, block_time)) = accumulate_prices(
        env,
        &config,
        pools[0].amount,
        query_token_precision(&deps.querier, pools[0].info.clone())?,
        pools[1].amount,
        query_token_precision(&deps.querier, pools[1].info.clone())?,
    )? {
        config.price0_cumulative_last = price0_cumulative_new;
        config.price1_cumulative_last = price1_cumulative_new;
        config.block_time_last = block_time;
        CONFIG.save(deps.storage, &config)?;
    }

    Ok(Response::new()
        .add_messages(
            // 1. send collateral token from the contract to a user
            // 2. send inactive commission to collector
            messages,
        )
        .add_attribute("action", "swap")
        .add_attribute("sender", sender.as_str())
        .add_attribute("receiver", receiver.as_str())
        .add_attribute("offer_asset", offer_asset.info.to_string())
        .add_attribute("ask_asset", ask_pool.info.to_string())
        .add_attribute("offer_amount", offer_amount.to_string())
        .add_attribute("return_amount", return_amount.to_string())
        .add_attribute("tax_amount", tax_amount.to_string())
        .add_attribute("spread_amount", spread_amount.to_string())
        .add_attribute("commission_amount", commission_amount.to_string())
        .add_attribute("maker_fee_amount", maker_fee_amount.to_string()))
}

/// ## Description
/// Accumulate token prices for the assets in the pool.
/// Note that this function shifts **block_time** when any of the token prices is zero in order to not
/// fill an accumulator with a null price for that period.
/// ## Params
/// * **env** is an object of type [`Env`].
///
/// * **config** is an object of type [`Config`].
///
/// * **x** is the balance of asset\[\0] in the pool.
///
/// * **x_precision** is the precision for the x token.
///
/// * **y** is the balance of asset\[\1] in the pool.
///
/// * **y_precision** is the precision for the y token.
pub fn accumulate_prices(
    env: Env,
    config: &Config,
    x: Uint128,
    x_precision: u8,
    y: Uint128,
    y_precision: u8,
) -> StdResult<Option<(Uint128, Uint128, u64)>> {
    let block_time = env.block.time.seconds();
    if block_time <= config.block_time_last {
        return Ok(None);
    }

    // We have to shift block_time when any price is zero in order to not fill an accumulator with a null price for that period
    let greater_precision = x_precision.max(y_precision).max(TWAP_PRECISION);
    let x = adjust_precision(x, x_precision, greater_precision)?;
    let y = adjust_precision(y, y_precision, greater_precision)?;

    let time_elapsed = Uint128::from(block_time - config.block_time_last);

    let mut pcl0 = config.price0_cumulative_last;
    let mut pcl1 = config.price1_cumulative_last;

    if !x.is_zero() && !y.is_zero() {
        let current_amp = compute_current_amp(config, &env)?;
        pcl0 = config.price0_cumulative_last.wrapping_add(adjust_precision(
            time_elapsed.checked_mul(Uint128::new(
                calc_ask_amount(
                    x.u128(),
                    y.u128(),
                    adjust_precision(Uint128::new(1), 0, greater_precision)?.u128(),
                    current_amp,
                )
                .unwrap(),
            ))?,
            greater_precision,
            TWAP_PRECISION,
        )?);
        pcl1 = config.price1_cumulative_last.wrapping_add(adjust_precision(
            time_elapsed.checked_mul(Uint128::new(
                calc_ask_amount(
                    y.u128(),
                    x.u128(),
                    adjust_precision(Uint128::new(1), 0, greater_precision)?.u128(),
                    current_amp,
                )
                .unwrap(),
            ))?,
            greater_precision,
            TWAP_PRECISION,
        )?)
    };

    Ok(Some((pcl0, pcl1, block_time)))
}

/// ## Description
/// Calculates the amount of fees the Maker contract gets according to specified pair parameters.
/// Returns a [`None`] if the Maker fee is zero, otherwise returns a [`Asset`] struct with the specified attributes.
/// ## Params
/// * **pool_info** is an object of type [`AssetInfo`]. Contains information about the pool asset for which the commission will be calculated.
///
/// * **commission_amount** is an object of type [`Env`]. This is the total amount of fees charged for a swap.
///
/// * **maker_commission_rate** is an object of type [`MessageInfo`]. This is the percentage of fees that go to the Maker contract.
pub fn calculate_maker_fee(
    pool_info: AssetInfo,
    commission_amount: Uint128,
    maker_commission_rate: Decimal,
) -> Option<Asset> {
    let maker_fee: Uint128 = commission_amount * maker_commission_rate;
    if maker_fee.is_zero() {
        return None;
    }

    Some(Asset {
        info: pool_info,
        amount: maker_fee,
    })
}

/// ## Description
/// Exposes all the queries available in the contract.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **_env** is an object of type [`Env`].
///
/// * **msg** is an object of type [`QueryMsg`].
///
/// ## Queries
/// * **QueryMsg::Pair {}** Returns information about the pair in an object of type [`PairInfo`].
///
/// * **QueryMsg::Pool {}** Returns information about the amount of assets in the pair contract as
/// well as the amount of LP tokens issued using an object of type [`PoolResponse`].
///
/// * **QueryMsg::Share { amount }** Returns the amount of assets that could be withdrawn from the pool
/// using a specific amount of LP tokens. The result is returned in a vector that contains objects of type [`Asset`].
///
/// * **QueryMsg::Simulation { offer_asset }** Returns the result of a swap simulation using a [`SimulationResponse`] object.
///
/// * **QueryMsg::ReverseSimulation { ask_asset }** Returns the result of a reverse swap simulation  using
/// a [`ReverseSimulationResponse`] object.
///
/// * **QueryMsg::CumulativePrices {}** Returns information about cumulative prices for the assets in the
/// pool using a [`CumulativePricesResponse`] object.
///
/// * **QueryMsg::Config {}** Returns the configuration for the pair contract using a [`ConfigResponse`] object.
///
/// * **QueryMsg::PendingReward {}** Returns the amount of bLUNA pending rewards for a specific address using an [`Asset`] object.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Pair {} => to_binary(&query_pair_info(deps)?),
        QueryMsg::Pool {} => to_binary(&query_pool(deps)?),
        QueryMsg::Share { amount } => to_binary(&query_share(deps, amount)?),
        QueryMsg::Simulation { offer_asset } => {
            to_binary(&query_simulation(deps, env, offer_asset)?)
        }
        QueryMsg::ReverseSimulation { ask_asset } => {
            to_binary(&query_reverse_simulation(deps, env, ask_asset)?)
        }
        QueryMsg::CumulativePrices {} => to_binary(&query_cumulative_prices(deps, env)?),
        QueryMsg::Config {} => to_binary(&query_config(deps, env)?),
        QueryMsg::PendingReward { user } => to_binary(&query_pending_reward(deps, env, user)?),
    }
}

/// ## Description
/// Returns information about the pair contract in an object of type [`PairInfo`].
/// ## Params
/// * **deps** is an object of type [`Deps`].
pub fn query_pair_info(deps: Deps) -> StdResult<PairInfo> {
    let config: Config = CONFIG.load(deps.storage)?;
    Ok(config.pair_info)
}

/// ## Description
/// Returns the amounts of assets in the pair contract as well as the amount of LP
/// tokens currently minted in an object of type [`PoolResponse`].
/// ## Params
/// * **deps** is an object of type [`Deps`].
pub fn query_pool(deps: Deps) -> StdResult<PoolResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let (assets, total_share) = pool_info(deps, config)?;

    let resp = PoolResponse {
        assets,
        total_share,
    };

    Ok(resp)
}

/// ## Description
/// Returns the amount of assets that could be withdrawn from the pool using a specific amount of LP tokens.
/// The result is returned in a vector that contains objects of type [`Asset`].
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **amount** is an object of type [`Uint128`]. This is the amount of LP tokens for which we calculate associated amounts of assets.
pub fn query_share(deps: Deps, amount: Uint128) -> StdResult<[Asset; 2]> {
    let config: Config = CONFIG.load(deps.storage)?;
    let (pools, total_share) = pool_info(deps, config)?;
    let refund_assets = get_share_in_assets(&pools, amount, total_share);

    Ok(refund_assets)
}

/// ## Description
/// Returns information about a swap simulation in a [`SimulationResponse`] object.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **offer_asset** is an object of type [`Asset`]. This is the asset to swap as well as an amount of the said asset.
pub fn query_simulation(deps: Deps, env: Env, offer_asset: Asset) -> StdResult<SimulationResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let contract_addr = config.pair_info.contract_addr.clone();

    let pools: [Asset; 2] = config.pair_info.query_pools(&deps.querier, contract_addr)?;

    let offer_pool: Asset;
    let ask_pool: Asset;
    if offer_asset.info.equal(&pools[0].info) {
        offer_pool = pools[0].clone();
        ask_pool = pools[1].clone();
    } else if offer_asset.info.equal(&pools[1].info) {
        offer_pool = pools[1].clone();
        ask_pool = pools[0].clone();
    } else {
        return Err(StdError::generic_err(
            "Given offer asset doesn't belong to pairs",
        ));
    }

    // Get fee info from factory
    let fee_info = query_fee_info(
        &deps.querier,
        config.factory_addr.clone(),
        config.pair_info.pair_type.clone(),
    )?;

    let (return_amount, spread_amount, commission_amount) = compute_swap(
        offer_pool.amount,
        query_token_precision(&deps.querier, offer_pool.info)?,
        ask_pool.amount,
        query_token_precision(&deps.querier, ask_pool.info)?,
        offer_asset.amount,
        fee_info.total_fee_rate,
        compute_current_amp(&config, &env)?,
    )?;

    Ok(SimulationResponse {
        return_amount,
        spread_amount,
        commission_amount,
    })
}

/// ## Description
/// Returns information about a reverse swap simulation in a [`ReverseSimulationResponse`] object.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **ask_asset** is an object of type [`Asset`]. This is the asset to swap to as well as the desired
/// amount of ask assets to receive from the swap.
pub fn query_reverse_simulation(
    deps: Deps,
    env: Env,
    ask_asset: Asset,
) -> StdResult<ReverseSimulationResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let contract_addr = config.pair_info.contract_addr.clone();

    let pools: [Asset; 2] = config.pair_info.query_pools(&deps.querier, contract_addr)?;

    let offer_pool: Asset;
    let ask_pool: Asset;
    if ask_asset.info.equal(&pools[0].info) {
        ask_pool = pools[0].clone();
        offer_pool = pools[1].clone();
    } else if ask_asset.info.equal(&pools[1].info) {
        ask_pool = pools[1].clone();
        offer_pool = pools[0].clone();
    } else {
        return Err(StdError::generic_err(
            "Given ask asset doesn't belong to pairs",
        ));
    }

    // Get fee info from the factory
    let fee_info = query_fee_info(
        &deps.querier,
        config.factory_addr.clone(),
        config.pair_info.pair_type.clone(),
    )?;

    let (offer_amount, spread_amount, commission_amount) = compute_offer_amount(
        offer_pool.amount,
        query_token_precision(&deps.querier, offer_pool.info)?,
        ask_pool.amount,
        query_token_precision(&deps.querier, ask_pool.info)?,
        ask_asset.amount,
        fee_info.total_fee_rate,
        compute_current_amp(&config, &env)?,
    )?;

    Ok(ReverseSimulationResponse {
        offer_amount,
        spread_amount,
        commission_amount,
    })
}

/// ## Description
/// Returns information about cumulative prices for the assets in the pool using a [`CumulativePricesResponse`] object.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **env** is an object of type [`Env`].
pub fn query_cumulative_prices(deps: Deps, env: Env) -> StdResult<CumulativePricesResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let (assets, total_share) = pool_info(deps, config.clone())?;

    let mut price0_cumulative_last = config.price0_cumulative_last;
    let mut price1_cumulative_last = config.price1_cumulative_last;

    if let Some((price0_cumulative_new, price1_cumulative_new, _)) = accumulate_prices(
        env,
        &config,
        assets[0].amount,
        query_token_precision(&deps.querier, assets[0].info.clone())?,
        assets[1].amount,
        query_token_precision(&deps.querier, assets[1].info.clone())?,
    )? {
        price0_cumulative_last = price0_cumulative_new;
        price1_cumulative_last = price1_cumulative_new;
    }

    let resp = CumulativePricesResponse {
        assets,
        total_share,
        price0_cumulative_last,
        price1_cumulative_last,
    };

    Ok(resp)
}

/// ## Description
/// Returns the pair contract configuration in a [`ConfigResponse`] object.
/// ## Params
/// * **deps** is an object of type [`Deps`].
pub fn query_config(deps: Deps, env: Env) -> StdResult<ConfigResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        block_time_last: config.block_time_last,
        params: Some(to_binary(&StablePoolConfig {
            amp: Decimal::from_ratio(compute_current_amp(&config, &env)?, AMP_PRECISION),
            bluna_rewarder: config.bluna_rewarder,
            generator: config.generator,
        })?),
    })
}

/// ## Description
/// Returns the amount of bLUNA pending rewards for a specific address using a [`Asset`] object.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **env** is an object of type [`Env`].
///
/// * **user** is an object of type [`String`]. This is the address for which we query the amount of pending bLUNA rewards to claim.
pub fn query_pending_reward(deps: Deps, _env: Env, user: String) -> StdResult<Asset> {
    use cosmwasm_std::Decimal256;

    let user = addr_validate_to_lower(deps.api, &user)?;

    let config = CONFIG.load(deps.storage)?;

    let user_share: Uint128 = deps.querier.query_wasm_smart(
        &config.generator,
        &GeneratorQueryMsg::Deposit {
            lp_token: config.pair_info.liquidity_token.to_string(),
            user: user.to_string(),
        },
    )?;

    let global_index = BLUNA_REWARD_GLOBAL_INDEX
        .may_load(deps.storage)?
        .unwrap_or_default();

    let user_index_opt = BLUNA_REWARD_USER_INDEXES.may_load(deps.storage, &user)?;

    let user_index = if let Some(user_index) = user_index_opt {
        user_index
    } else if user_share.is_zero() {
        global_index
    } else {
        Decimal256::zero()
    };

    Ok(Asset {
        info: AssetInfo::NativeToken {
            denom: "uusd".to_string(),
        },
        amount: ((global_index - user_index) * Uint256::from(user_share)).try_into()?,
    })
}

/// ## Description
/// Returns an amount of coins. For each coin in the specified vector, if the coin is null, we return `Uint128::zero()`,
/// otherwise we return the specified coin amount.
/// ## Params
/// * **coins** is an array of [`Coin`] type items. This is a list of coins for which we return amounts.
///
/// * **denom** is an object of type [`String`]. This is the denomination used for the coins.
pub fn amount_of(coins: &[Coin], denom: String) -> Uint128 {
    match coins.iter().find(|x| x.denom == denom) {
        Some(coin) => coin.amount,
        None => Uint128::zero(),
    }
}

/// ## Description
/// Returns the result of a swap.
/// ## Params
/// * **offer_pool** is an object of type [`Uint128`]. This is the total amount of offer assets in the pool.
///
/// * **offer_precision** is an object of type [`u8`]. This is the token precision used for the offer amount.
///
/// * **ask_pool** is an object of type [`Uint128`]. This is the total amount of ask assets in the pool.
///
/// * **ask_precision** is an object of type [`u8`]. This is the token precision used for the ask amount.
///
/// * **offer_amount** is an object of type [`Uint128`]. This is the amount of offer assets to swap.
///
/// * **commission_rate** is an object of type [`Decimal`]. This is the total amount of fees charged for the swap.
///
/// * **amp** is an object of type [`u64`]. This is the pool amplification used to calculate the swap result.
fn compute_swap(
    offer_pool: Uint128,
    offer_precision: u8,
    ask_pool: Uint128,
    ask_precision: u8,
    offer_amount: Uint128,
    commission_rate: Decimal,
    amp: u64,
) -> StdResult<(Uint128, Uint128, Uint128)> {
    // offer => ask

    let greater_precision = offer_precision.max(ask_precision);
    let offer_pool = adjust_precision(offer_pool, offer_precision, greater_precision)?;
    let ask_pool = adjust_precision(ask_pool, ask_precision, greater_precision)?;
    let offer_amount = adjust_precision(offer_amount, offer_precision, greater_precision)?;

    let return_amount = Uint128::new(
        calc_ask_amount(offer_pool.u128(), ask_pool.u128(), offer_amount.u128(), amp).unwrap(),
    );

    // We assume the assets should stay in a 1:1 ratio, so the true exchange rate is 1. So any exchange rate <1 could be considered the spread
    let spread_amount = offer_amount.saturating_sub(return_amount);

    let commission_amount: Uint128 = return_amount * commission_rate;

    // The commission will be absorbed by the pool
    let return_amount: Uint128 = return_amount.checked_sub(commission_amount).unwrap();

    let return_amount = adjust_precision(return_amount, greater_precision, ask_precision)?;
    let spread_amount = adjust_precision(spread_amount, greater_precision, ask_precision)?;
    let commission_amount = adjust_precision(commission_amount, greater_precision, ask_precision)?;

    Ok((return_amount, spread_amount, commission_amount))
}

/// ## Description
/// Returns an amount of offer assets for a specified amount of ask assets.
/// ## Params
/// * **offer_pool** is an object of type [`Uint128`]. This is the total amount of offer assets in the pool.
///
/// * **offer_precision** is an object of type [`u8`]. This is the token precision used for the offer amount.
///
/// * **ask_pool** is an object of type [`Uint128`]. This is the total amount of ask assets in the pool.
///
/// * **ask_precision** is an object of type [`u8`]. This is the token precision used for the ask amount.
///
/// * **ask_amount** is an object of type [`Uint128`]. This is the amount of ask assets to swap to.
///
/// * **commission_rate** is an object of type [`Decimal`]. This is the total amount of fees charged for the swap.
fn compute_offer_amount(
    offer_pool: Uint128,
    offer_precision: u8,
    ask_pool: Uint128,
    ask_precision: u8,
    ask_amount: Uint128,
    commission_rate: Decimal,
    amp: u64,
) -> StdResult<(Uint128, Uint128, Uint128)> {
    // ask => offer

    let greater_precision = offer_precision.max(ask_precision);
    let offer_pool = adjust_precision(offer_pool, offer_precision, greater_precision)?;
    let ask_pool = adjust_precision(ask_pool, ask_precision, greater_precision)?;
    let ask_amount = adjust_precision(ask_amount, ask_precision, greater_precision)?;

    let one_minus_commission = Decimal256::one() - Decimal256::from(commission_rate);
    let inv_one_minus_commission: Decimal = (Decimal256::one() / one_minus_commission).into();
    let before_commission_deduction = ask_amount * inv_one_minus_commission;

    let offer_amount = Uint128::new(
        calc_offer_amount(
            offer_pool.u128(),
            ask_pool.u128(),
            before_commission_deduction.u128(),
            amp,
        )
        .unwrap(),
    );

    // We assume the assets should stay in a 1:1 ratio, so the true exchange rate is 1. So any exchange rate <1 could be considered the spread
    let spread_amount = offer_amount.saturating_sub(before_commission_deduction);

    let commission_amount = before_commission_deduction * commission_rate;

    let offer_amount = adjust_precision(offer_amount, greater_precision, offer_precision)?;
    let spread_amount = adjust_precision(spread_amount, greater_precision, ask_precision)?;
    let commission_amount = adjust_precision(commission_amount, greater_precision, ask_precision)?;

    Ok((offer_amount, spread_amount, commission_amount))
}

/// ## Description
/// Return a value using a newly specified precision.
/// ## Params
/// * **value** is an object of type [`Uint128`]. This is the value that will have its precision adjusted.
///
/// * **current_precision** is an object of type [`u8`]. This is the `value`'s current precision
///
/// * **new_precision** is an object of type [`u8`]. This is the new precision to use when returning the `value`.
fn adjust_precision(
    value: Uint128,
    current_precision: u8,
    new_precision: u8,
) -> StdResult<Uint128> {
    Ok(match current_precision.cmp(&new_precision) {
        Ordering::Equal => value,
        Ordering::Less => value.checked_mul(Uint128::new(
            10_u128.pow((new_precision - current_precision) as u32),
        ))?,
        Ordering::Greater => value.checked_div(Uint128::new(
            10_u128.pow((current_precision - new_precision) as u32),
        ))?,
    })
}

/// ## Description
/// Returns a [`ContractError`] on failure.
/// If `belief_price` and `max_spread` are both specified, we compute a new spread,
/// otherwise we just use the swap spread to check `max_spread`.
/// ## Params
/// * **belief_price** is an object of type [`Option<Decimal>`]. This is the belief price used in the swap.
///
/// * **max_spread** is an object of type [`Option<Decimal>`]. This is the
/// max spread allowed so that the swap can be executed successfuly.
///
/// * **offer_amount** is an object of type [`Uint128`]. This is the amount of assets to swap.
///
/// * **return_amount** is an object of type [`Uint128`]. This is the amount of assets to receive from the swap.
///
/// * **spread_amount** is an object of type [`Uint128`]. This is the spread used in the swap.
pub fn assert_max_spread(
    belief_price: Option<Decimal>,
    max_spread: Option<Decimal>,
    offer_amount: Uint128,
    return_amount: Uint128,
    spread_amount: Uint128,
) -> Result<(), ContractError> {
    let default_spread = Decimal::from_str(DEFAULT_SLIPPAGE)?;
    let max_allowed_spread = Decimal::from_str(MAX_ALLOWED_SLIPPAGE)?;

    let max_spread = max_spread.unwrap_or(default_spread);
    if max_spread.gt(&max_allowed_spread) {
        return Err(ContractError::AllowedSpreadAssertion {});
    }

    if let Some(belief_price) = belief_price {
        let expected_return =
            offer_amount * Decimal::from(Decimal256::one() / Decimal256::from(belief_price));
        let spread_amount = expected_return
            .checked_sub(return_amount)
            .unwrap_or_else(|_| Uint128::zero());

        if return_amount < expected_return
            && Decimal::from_ratio(spread_amount, expected_return) > max_spread
        {
            return Err(ContractError::MaxSpreadAssertion {});
        }
    } else if Decimal::from_ratio(spread_amount, return_amount + spread_amount) > max_spread {
        return Err(ContractError::MaxSpreadAssertion {});
    }

    Ok(())
}

/// ## Description
/// This is an internal function that enforces slippage tolerance for swaps.
/// Returns a [`ContractError`] on failure, otherwise returns [`Ok`].
/// ## Params
/// * **slippage_tolerance** is an object of type [`Option<Decimal>`]. This is the slippage tolerance to enforce.
///
/// * **deposits** are an array of [`Uint128`] type items. These are offer and ask amounts for a swap.
///
/// * **pools** are an array of [`Asset`] type items. These are total amounts of assets in the pool.
fn assert_slippage_tolerance(
    _slippage_tolerance: &Option<Decimal>,
    _deposits: &[Uint128; 2],
    _pools: &[Asset; 2],
) -> Result<(), ContractError> {
    //There is no slippage in the stable pool
    Ok(())
}

/// ## Description
/// Used for the contract migration. Returns a default object of type [`Response`].
/// ## Params
/// * **_deps** is an object of type [`DepsMut`].
///
/// * **_env** is an object of type [`Env`].
///
/// * **_msg** is an object of type [`MigrateMsg`].
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, env: Env, msg: MigrateMsg) -> Result<Response, ContractError> {
    let contract_version = get_contract_version(deps.storage)?;

    let mut response = Response::new()
        .add_attribute("previous_contract_name", &contract_version.contract)
        .add_attribute("previous_contract_version", &contract_version.version);

    match contract_version.contract.as_ref() {
        "astroport-pair-stable" => match contract_version.version.as_ref() {
            "1.0.0" => {
                let mut config = CONFIG.load(deps.storage)?;
                config.bluna_rewarder = addr_validate_to_lower(deps.api, &msg.bluna_rewarder)?;
                config.generator = addr_validate_to_lower(deps.api, &msg.generator)?;
                CONFIG.save(deps.storage, &config)?;
                response
                    .messages
                    .push(get_bluna_reward_holder_instantiating_message(
                        deps.as_ref(),
                        &env,
                        &config.factory_addr,
                    )?);
            }
            _ => return Err(ContractError::MigrationError {}),
        },
        _ => return Err(ContractError::MigrationError {}),
    };

    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    Ok(response
        .add_attribute("new_contract_name", CONTRACT_NAME)
        .add_attribute("new_contract_version", CONTRACT_VERSION))
}

/// ## Description
/// Returns information about the pool.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **config** is an object of type [`Config`].
pub fn pool_info(deps: Deps, config: Config) -> StdResult<([Asset; 2], Uint128)> {
    let contract_addr = config.pair_info.contract_addr.clone();
    let pools: [Asset; 2] = config.pair_info.query_pools(&deps.querier, contract_addr)?;
    let total_share: Uint128 = query_supply(&deps.querier, config.pair_info.liquidity_token)?;

    Ok((pools, total_share))
}

/// ## Description
/// Updates the pool configuration with the specified parameters in the `params` variable.
/// Returns a [`ContractError`] as a failure, otherwise returns a [`Response`] with the specified
/// attributes if the operation was successful
/// ## Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **params** is an object of type [`Binary`]. These are the the new parameter values.
pub fn update_config(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    params: Binary,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let factory_config = query_factory_config(&deps.querier, config.factory_addr.clone())?;

    if info.sender != factory_config.owner {
        return Err(ContractError::Unauthorized {});
    }

    match from_binary::<StablePoolUpdateParams>(&params)? {
        StablePoolUpdateParams::StartChangingAmp {
            next_amp,
            next_amp_time,
        } => start_changing_amp(config, deps, env, next_amp, next_amp_time)?,
        StablePoolUpdateParams::StopChangingAmp {} => stop_changing_amp(config, deps, env)?,
        StablePoolUpdateParams::BlunaRewarder { address } => {
            let address = addr_validate_to_lower(deps.as_ref().api, &address)?;
            CONFIG.update::<_, StdError>(deps.storage, |mut cfg| {
                cfg.bluna_rewarder = address;
                Ok(cfg)
            })?;
        }
    }

    Ok(Response::default())
}

/// ## Description
/// Start changing the AMP value. Returns a [`ContractError`] on failure, otherwise returns [`Ok`].
/// ## Params
/// * **mut config** is an object of type [`Config`]. This is a mutable reference to the pool configuration.
///
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **next_amp** is an object of type [`u64`]. This is the new value for AMP.
///
/// * **next_amp_time** is an object of type [`u64`]. This is the end time when the pool amplification will be equal to `next_amp`.
fn start_changing_amp(
    mut config: Config,
    deps: DepsMut,
    env: Env,
    next_amp: u64,
    next_amp_time: u64,
) -> Result<(), ContractError> {
    if next_amp == 0 || next_amp > MAX_AMP {
        return Err(ContractError::IncorrectAmp {});
    }

    let current_amp = compute_current_amp(&config, &env)?;

    let next_amp_with_precision = next_amp * AMP_PRECISION;

    if next_amp_with_precision * MAX_AMP_CHANGE < current_amp
        || next_amp_with_precision > current_amp * MAX_AMP_CHANGE
    {
        return Err(ContractError::MaxAmpChangeAssertion {});
    }

    let block_time = env.block.time.seconds();

    if block_time < config.init_amp_time + MIN_AMP_CHANGING_TIME
        || next_amp_time < block_time + MIN_AMP_CHANGING_TIME
    {
        return Err(ContractError::MinAmpChangingTimeAssertion {});
    }

    config.init_amp = current_amp;
    config.next_amp = next_amp_with_precision;
    config.init_amp_time = block_time;
    config.next_amp_time = next_amp_time;

    CONFIG.save(deps.storage, &config)?;

    Ok(())
}

/// ## Description
/// Stop changing the AMP value. Returns [`Ok`].
/// ## Params
/// * **mut config** is an object of type [`Config`]. This is a mutable reference to the pool configuration.
///
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
fn stop_changing_amp(mut config: Config, deps: DepsMut, env: Env) -> StdResult<()> {
    let current_amp = compute_current_amp(&config, &env)?;
    let block_time = env.block.time.seconds();

    config.init_amp = current_amp;
    config.next_amp = current_amp;
    config.init_amp_time = block_time;
    config.next_amp_time = block_time;

    // now (block_time < next_amp_time) is always False, so we return saved Amp
    CONFIG.save(deps.storage, &config)?;

    Ok(())
}

/// ## Description
/// Compute the current amplification coefficient (AMP)
/// ## Params
/// * **config** is an object of type [`Config`].
///
/// * **env** is an object of type [`Env`].
fn compute_current_amp(config: &Config, env: &Env) -> StdResult<u64> {
    let block_time = env.block.time.seconds();

    if block_time < config.next_amp_time {
        let elapsed_time =
            Uint128::from(block_time).checked_sub(Uint128::from(config.init_amp_time))?;
        let time_range =
            Uint128::from(config.next_amp_time).checked_sub(Uint128::from(config.init_amp_time))?;
        let init_amp = Uint128::from(config.init_amp);
        let next_amp = Uint128::from(config.next_amp);

        if config.next_amp > config.init_amp {
            let amp_range = next_amp - init_amp;
            let res = init_amp + (amp_range * elapsed_time).checked_div(time_range)?;
            Ok(res.u128() as u64)
        } else {
            let amp_range = init_amp - next_amp;
            let res = init_amp - (amp_range * elapsed_time).checked_div(time_range)?;
            Ok(res.u128() as u64)
        }
    } else {
        Ok(config.next_amp)
    }
}

/// ## Description
/// Return a message object that can help claim bLUNA rewards for an account.
/// Returns an [`ContractError`] on failure, otherwise returns the object
/// of type [`SubMsg`].
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **env** is an object of type [`Env`].
///
/// * **factory_addr** is an object of type [`Addr`]. This is the Astroport factory address.
fn get_bluna_reward_holder_instantiating_message(
    deps: Deps,
    env: &Env,
    factory_addr: &Addr,
) -> Result<SubMsg, ContractError> {
    Ok(SubMsg {
        msg: CosmosMsg::Wasm(WasmMsg::Instantiate {
            admin: None,
            code_id: query_factory_config(&deps.querier, factory_addr.clone())?.whitelist_code_id,
            funds: vec![],
            label: "Bluna rewarder".to_string(),
            msg: to_binary(&WhitelistInstantiateMsg {
                admins: vec![env.contract.address.to_string()],
                mutable: false,
            })?,
        }),
        id: INSTANTIATE_BLUNA_REWARD_HOLDER_REPLY_ID,
        gas_limit: None,
        reply_on: ReplyOn::Success,
    })
}

/// ## Description
/// Returns a vector of messages that are meant to claim bLUNA rewards for a specific user.
/// Returns a [`ContractError`] on failure, otherwise returns the vector that contains objects
/// of type [`CosmosMsg`].
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **env** is an object of type [`Env`].
///
/// * **bluna_rewarder** is an object of type [`str`]. This is the contract that distributes bLUNA rewards.
///
/// * **user** is an object of type [`Addr`]. This is the address for which we return messages that are meant to claim rewards.
///
/// * **user_share** is an object of type [`Uint128`]. This is the share of the bLUNA rewards that the `user` is entitled to.
///
/// * **total_share** is an object of type [`Uint128`]. This is the total amount of bLUNA rewards entitled to bLUNA LPs.
///
/// * **receiver** is an object of type [`Option<Addr>`]. This is a custom address that can receive the bLUNA rewards.
/// If it's not present, the function defaults to the `user`.
fn get_bluna_reward_handling_messages(
    deps: Deps,
    env: &Env,
    bluna_rewarder: &str,
    user: Addr,
    user_share: Uint128,
    total_share: Uint128,
    receiver: Option<Addr>,
) -> Result<Vec<CosmosMsg>, ContractError> {
    let bluna_reward_holder = BLUNA_REWARD_HOLDER.load(deps.storage)?;

    let reward_balance = astroport::querier::query_balance(
        &deps.querier,
        bluna_reward_holder.clone(),
        "uusd".to_string(),
    )?;

    Ok(vec![
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: bluna_rewarder.to_string(),
            msg: to_binary(&anchor_basset::reward::ExecuteMsg::ClaimRewards {
                recipient: Some(bluna_reward_holder.to_string()),
            })?,
            funds: vec![],
        }),
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: env.contract.address.to_string(),
            funds: vec![],
            msg: to_binary(&ExecuteMsg::HandleReward {
                previous_reward_balance: reward_balance,
                user,
                user_share,
                total_share,
                receiver,
            })?,
        }),
    ])
}

/// ## Description
/// Claims bLUNA rewards on behalf of the function caller and sends them to the specified receiver.
/// Returns A [`ContractError`] on failure, otherwise returns A [`Response`] with the specified attributes if the operation was successful.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **receiver** is an object of type [`Option<String>`]. This is the address that receives the bLUNA rewards.
fn claim_reward(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    receiver: Option<String>,
) -> Result<Response, ContractError> {
    let receiver = receiver
        .map(|receiver| addr_validate_to_lower(deps.api, &receiver))
        .transpose()?;

    let config: Config = CONFIG.load(deps.storage)?;

    let user_share: Uint128 = deps.querier.query_wasm_smart(
        &config.generator,
        &GeneratorQueryMsg::Deposit {
            lp_token: config.pair_info.liquidity_token.to_string(),
            user: info.sender.to_string(),
        },
    )?;

    if user_share.is_zero() {
        return Err(StdError::generic_err("No lp tokens staked to the generator!").into());
    }

    let pool_info: PoolInfoResponse = deps.querier.query_wasm_smart(
        &config.generator,
        &GeneratorQueryMsg::PoolInfo {
            lp_token: config.pair_info.liquidity_token.to_string(),
        },
    )?;

    Ok(
        Response::new().add_messages(get_bluna_reward_handling_messages(
            deps.as_ref(),
            &env,
            config.bluna_rewarder.as_str(),
            info.sender,
            user_share,
            pool_info.lp_supply,
            receiver,
        )?),
    )
}

/// ## Description
/// Claims bLUNA rewards on behalf of stakers that deposited their LP tokens in the Generator contract.
/// Returns a [`ContractError`] on failure, otherwise returns a [`Response`] with the
/// specified attributes if the operation was successful.
/// ## Params
/// * **deps** is an object of type [`Deps`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **user** is an object of type [`String`].
/// This is the user for which to claim bLUNA rewards that were accrued by the Generator on their behalf.
///
/// * **user_share** is an object of type [`Uint128`]. This is the user's share of bLUNA rewards.
///
/// * **total_share** is an object of type [`Uint128`]. This is the total share of bLUNA rewards that need to be distributed.
fn claim_reward_by_generator(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    user: String,
    user_share: Uint128,
    total_share: Uint128,
) -> Result<Response, ContractError> {
    let config: Config = CONFIG.load(deps.storage)?;

    let user = addr_validate_to_lower(deps.api, &user)?;

    if info.sender != config.generator {
        return Err(StdError::generic_err("Only the generator can use this method!").into());
    }

    Ok(
        Response::new().add_messages(get_bluna_reward_handling_messages(
            deps.as_ref(),
            &env,
            config.bluna_rewarder.as_str(),
            user,
            user_share,
            total_share,
            None,
        )?),
    )
}

/// ## Description
/// Handles and distributes bLUNA rewards.
/// Returns a [`ContractError`] on failure, otherwise returns a [`Response`] with the
/// specified attributes if the operation was successful.
/// ## Params
/// * **deps** is an object of type [`DepsMut`].
///
/// * **env** is an object of type [`Env`].
///
/// * **info** is an object of type [`MessageInfo`].
///
/// * **previous_reward_balance** is an object of type [`Uint128`].
/// This is the previous bLUNA rewards balance that had to be distributed to LPs.
///
/// * **user** is an object of type [`Addr`]. This is the address for which we distribute bLUNA rewards.
///
/// * **user_share** is an object of type [`Uint128`]. This is the user's share of bLUNA rewards.
///
/// * **total_share** is an object of type [`Uint128`]. This is the total share of bLUNA rewards that need to be distributed.
///
/// * **receiver** is an object of type [`Option<Addr>`]. This is the address that will receive bLUNA rewards.
#[allow(clippy::too_many_arguments)]
pub fn handle_reward(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    previous_reward_balance: Uint128,
    user: Addr,
    user_share: Uint128,
    total_share: Uint128,
    receiver: Option<Addr>,
) -> Result<Response, ContractError> {
    use astroport::whitelist::ExecuteMsg;

    if info.sender != env.contract.address {
        return Err(ContractError::Unauthorized {});
    }

    let receiver = receiver.unwrap_or_else(|| user.clone());

    let bluna_reward_holder = BLUNA_REWARD_HOLDER.load(deps.storage)?;

    let reward_balance = astroport::querier::query_balance(
        &deps.querier,
        bluna_reward_holder.clone(),
        "uusd".to_string(),
    )?;

    let bluna_reward_global_index = BLUNA_REWARD_GLOBAL_INDEX
        .may_load(deps.storage)?
        .unwrap_or_default();
    let bluna_reward_user_index = BLUNA_REWARD_USER_INDEXES.may_load(deps.storage, &user)?;

    let (bluna_reward_global_index, latest_reward_amount, user_reward) = calc_user_reward(
        reward_balance,
        previous_reward_balance,
        user_share,
        total_share,
        bluna_reward_global_index,
        bluna_reward_user_index,
    )?;

    BLUNA_REWARD_GLOBAL_INDEX.save(deps.storage, &bluna_reward_global_index)?;
    BLUNA_REWARD_USER_INDEXES.save(deps.storage, &user, &bluna_reward_global_index)?;

    let mut response =
        Response::new().add_attribute("bluna_claimed_reward_to_pool", latest_reward_amount);

    if !user_reward.is_zero() {
        response.messages.push(SubMsg::new(WasmMsg::Execute {
            contract_addr: bluna_reward_holder.to_string(),
            funds: vec![],
            msg: to_binary(&ExecuteMsg::Execute {
                msgs: vec![Asset {
                    info: AssetInfo::NativeToken {
                        denom: "uusd".to_string(),
                    },
                    amount: user_reward,
                }
                .into_msg(&deps.querier, receiver.clone())?],
            })?,
        }));
    }

    Ok(response
        .add_attribute("user", user)
        .add_attribute("receiver", receiver)
        .add_attribute("sent_bluna_reward", user_reward))
}

/// ## Description
/// Calculate the amount of bLUNA rewards that a user can claim.
/// Returns a [`ContractError`] on failure, otherwise returns the tuple values
/// [bluna_reward_global_index, latest_reward_amount and user_reward].
/// ## Params
/// * **reward_balance** is an object of type [`Uint128`].
/// This is the total amount of bLUNA rewards that the bLUNA reward distributor currently holds.
///
/// * **previous_reward_balance** is an object of type [`Uint128`].
/// This is the total amount of bLUNA rewards that the bLUNA reward distributor previously held.
///
/// * **user_share** is an object of type [`Uint128`]. This is the portion of bLUNA rewards that the user can claim.
///
/// * **total_share** is an object of type [`Uint128`]. This is the total share of bLUNA rewards that go to bLUNA Astroport LPs.
///
/// * **bluna_reward_global_index** is an object of type [`Decimal256`].
/// This is an index tracking how many rewards have been distributed to bLUNA stakers.
///
/// * **bluna_reward_user_index** is an object of type [`Option<Decimal256>`].
/// This is an index tracking how many rewards have been claimed by the `user`.
pub fn calc_user_reward(
    reward_balance: Uint128,
    previous_reward_balance: Uint128,
    user_share: Uint128,
    total_share: Uint128,
    bluna_reward_global_index: cosmwasm_std::Decimal256,
    bluna_reward_user_index: Option<cosmwasm_std::Decimal256>,
) -> Result<(cosmwasm_std::Decimal256, Uint128, Uint128), ContractError> {
    use cosmwasm_std::Decimal256;

    let latest_reward_amount = reward_balance.saturating_sub(previous_reward_balance);

    let bluna_reward_global_index =
        bluna_reward_global_index + Decimal256::from_ratio(latest_reward_amount, total_share);

    let user_reward: Uint128 = if let Some(bluna_reward_user_index) = bluna_reward_user_index {
        ((bluna_reward_global_index - bluna_reward_user_index) * Uint256::from(user_share))
            .try_into()
            .map_err(|e| ContractError::Std(StdError::from(e)))?
    } else if !user_share.is_zero() {
        (bluna_reward_global_index * Uint256::from(user_share))
            .try_into()
            .map_err(|e| ContractError::Std(StdError::from(e)))?
    } else {
        Uint128::zero()
    };

    Ok((bluna_reward_global_index, latest_reward_amount, user_reward))
}
