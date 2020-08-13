use schemars::JsonSchema;
use std::fmt;

use cosmwasm_std::{
    log, to_binary, Api, BankMsg, Binary, Coin, CosmosMsg, Empty, Env, Extern, HandleResponse,
    HumanAddr, InitResponse, Querier, StdError, StdResult, Storage,
};
use cw1_whitelist::{
    contract::{handle_freeze, handle_update_admins, init as whitelist_init, query_admin_list},
    msg::InitMsg,
    state::admin_list_read,
};
use cw2::{set_contract_version, ContractVersion};
use cw20::Expiration;

use crate::msg::{HandleMsg, QueryMsg};
use crate::state::{allowances, allowances_read, Allowance};
use std::ops::{AddAssign, Sub};

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:cw1-subkeys";
const CONTRACT_VERSION: &str = "v0.1.0";

pub fn init<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msg: InitMsg,
) -> StdResult<InitResponse> {
    let version = ContractVersion {
        contract: CONTRACT_NAME.to_string(),
        version: CONTRACT_VERSION.to_string(),
    };
    set_contract_version(&mut deps.storage, &version)?;
    whitelist_init(deps, env, msg)
}

pub fn handle<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    // Note: implement this function with different type to add support for custom messages
    // and then import the rest of this contract code.
    msg: HandleMsg<Empty>,
) -> StdResult<HandleResponse<Empty>> {
    match msg {
        HandleMsg::Execute { msgs } => handle_execute(deps, env, msgs),
        HandleMsg::Freeze {} => handle_freeze(deps, env),
        HandleMsg::UpdateAdmins { admins } => handle_update_admins(deps, env, admins),
        HandleMsg::IncreaseAllowance {
            spender,
            amount,
            expires,
        } => handle_increase_allowance(deps, env, spender, amount, expires),
        HandleMsg::DecreaseAllowance {
            spender,
            amount,
            expires,
        } => handle_decrease_allowance(deps, env, spender, amount, expires),
    }
}

pub fn handle_execute<S: Storage, A: Api, Q: Querier, T>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msgs: Vec<CosmosMsg<T>>,
) -> StdResult<HandleResponse<T>>
where
    T: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    let cfg = admin_list_read(&deps.storage).load()?;
    let owner_raw = &deps.api.canonical_address(&env.message.sender)?;
    // this is the admin behavior (same as cw1-whitelist)
    if cfg.is_admin(owner_raw) {
        let mut res = HandleResponse::default();
        res.messages = msgs;
        res.log = vec![log("action", "execute"), log("owner", env.message.sender)];
        Ok(res)
    } else {
        let mut allowances = allowances(&mut deps.storage);
        let allow = allowances.may_load(owner_raw.as_slice())?;
        let mut allowance =
            allow.ok_or_else(|| StdError::not_found("No allowance for this account"))?;
        for msg in &msgs {
            match msg {
                CosmosMsg::Bank(BankMsg::Send {
                    from_address: _,
                    to_address: _,
                    amount,
                }) => {
                    // Decrease allowance
                    for coin in amount {
                        allowance.balance = allowance.balance.sub(coin.clone())?;
                        // Fails if not enough tokens
                    }
                    allowances.save(owner_raw.as_slice(), &allowance)?;
                }
                _ => {
                    return Err(StdError::generic_err("Message type rejected"));
                }
            }
        }
        // Relay messages
        let res = HandleResponse {
            messages: msgs,
            log: vec![log("action", "execute"), log("owner", env.message.sender)],
            data: None,
        };
        Ok(res)
    }
}

pub fn handle_increase_allowance<S: Storage, A: Api, Q: Querier, T>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    spender: HumanAddr,
    amount: Coin,
    expires: Option<Expiration>,
) -> StdResult<HandleResponse<T>>
where
    T: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    let cfg = admin_list_read(&deps.storage).load()?;
    let spender_raw = &deps.api.canonical_address(&spender)?;
    let owner_raw = &deps.api.canonical_address(&env.message.sender)?;

    if !cfg.is_admin(&owner_raw) {
        return Err(StdError::unauthorized());
    }
    if spender_raw == owner_raw {
        return Err(StdError::generic_err("Cannot set allowance to own account"));
    }

    allowances(&mut deps.storage).update(spender_raw.as_slice(), |allow| {
        let mut allowance = allow.unwrap_or_default();
        if let Some(exp) = expires {
            allowance.expires = exp;
        }
        allowance.balance.add_assign(amount.clone());
        Ok(allowance)
    })?;

    let res = HandleResponse {
        messages: vec![],
        log: vec![
            log("action", "increase_allowance"),
            log("owner", env.message.sender),
            log("spender", spender),
            log("denomination", amount.denom),
            log("amount", amount.amount),
        ],
        data: None,
    };
    Ok(res)
}

pub fn handle_decrease_allowance<S: Storage, A: Api, Q: Querier, T>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    spender: HumanAddr,
    amount: Coin,
    expires: Option<Expiration>,
) -> StdResult<HandleResponse<T>>
where
    T: Clone + fmt::Debug + PartialEq + JsonSchema,
{
    let cfg = admin_list_read(&deps.storage).load()?;
    let spender_raw = &deps.api.canonical_address(&spender)?;
    let owner_raw = &deps.api.canonical_address(&env.message.sender)?;

    if !cfg.is_admin(&owner_raw) {
        return Err(StdError::unauthorized());
    }
    if spender_raw == owner_raw {
        return Err(StdError::generic_err("Cannot set allowance to own account"));
    }

    let allowance = allowances(&mut deps.storage).update(spender_raw.as_slice(), |allow| {
        // Fail fast
        let mut allowance =
            allow.ok_or_else(|| StdError::not_found("No allowance for this account"))?;
        if let Some(exp) = expires {
            allowance.expires = exp;
        }
        allowance.balance = allowance.balance.sub_saturating(amount.clone())?; // Tolerates underflows (amount bigger than balance), but fails if there are no tokens at all for the denom (report potential errors)
        Ok(allowance)
    })?;
    if allowance.balance.is_empty() {
        allowances(&mut deps.storage).remove(spender_raw.as_slice());
    }

    let res = HandleResponse {
        messages: vec![],
        log: vec![
            log("action", "decrease_allowance"),
            log("owner", deps.api.human_address(owner_raw)?),
            log("spender", spender),
            log("denomination", amount.denom),
            log("amount", amount.amount),
        ],
        data: None,
    };
    Ok(res)
}

pub fn query<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    msg: QueryMsg,
) -> StdResult<Binary> {
    match msg {
        QueryMsg::AdminList {} => to_binary(&query_admin_list(deps)?),
        QueryMsg::Allowance { spender } => to_binary(&query_allowance(deps, spender)?),
    }
}

// if the subkey has no allowance, return an empty struct (not an error)
pub fn query_allowance<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    spender: HumanAddr,
) -> StdResult<Allowance> {
    let subkey = deps.api.canonical_address(&spender)?;
    let allow = allowances_read(&deps.storage)
        .may_load(subkey.as_slice())?
        .unwrap_or_default();
    Ok(allow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::Balance;
    use cosmwasm_std::testing::{mock_dependencies, mock_env, MOCK_CONTRACT_ADDR};
    use cosmwasm_std::{coin, coins};
    use cw1_whitelist::msg::AdminListResponse;

    // this will set up the init for other tests
    fn setup_test_case<S: Storage, A: Api, Q: Querier>(
        mut deps: &mut Extern<S, A, Q>,
        env: &Env,
        admins: &Vec<HumanAddr>,
        spenders: &Vec<HumanAddr>,
        allowances: &Vec<Coin>,
        expirations: &Vec<Expiration>,
    ) {
        // Init a contract with admins
        let init_msg = InitMsg {
            admins: admins.clone(),
            mutable: true,
        };
        init(deps, env.clone(), init_msg).unwrap();

        // Add subkeys with initial allowances
        for (spender, expiration) in spenders.iter().zip(expirations) {
            for amount in allowances {
                let msg = HandleMsg::IncreaseAllowance {
                    spender: spender.clone(),
                    amount: amount.clone(),
                    expires: Some(expiration.clone()),
                };
                handle(&mut deps, env.clone(), msg).unwrap();
            }
        }
    }

    #[test]
    fn query_allowances() {
        let mut deps = mock_dependencies(20, &coins(1111, "token1"));

        let owner = HumanAddr::from("admin0001");
        let admins = vec![owner.clone(), HumanAddr::from("admin0002")];

        let spender1 = HumanAddr::from("spender0001");
        let spender2 = HumanAddr::from("spender0002");
        let spender3 = HumanAddr::from("spender0003");
        let initial_spenders = vec![spender1.clone(), spender2.clone()];

        // Same allowances for all spenders, for simplicity
        let denom1 = "token1";
        let amount1 = 1111;

        let allow1 = coin(amount1, denom1);
        let initial_allowances = vec![allow1.clone()];

        let expires_never = Expiration::Never {};
        let initial_expirations = vec![expires_never.clone(), expires_never.clone()];

        let env = mock_env(owner, &[]);
        setup_test_case(
            &mut deps,
            &env,
            &admins,
            &initial_spenders,
            &initial_allowances,
            &initial_expirations,
        );

        // Check allowances work for accounts with balances
        let allowance = query_allowance(&deps, spender1.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow1.clone()]),
                expires: expires_never.clone()
            }
        );
        let allowance = query_allowance(&deps, spender2.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow1.clone()]),
                expires: expires_never.clone()
            }
        );

        // Check allowances work for accounts with no balance
        let allowance = query_allowance(&deps, spender3.clone()).unwrap();
        assert_eq!(allowance, Allowance::default(),);
    }

    #[test]
    fn update_admins_and_query() {
        let mut deps = mock_dependencies(20, &coins(1111, "token1"));

        let owner = HumanAddr::from("admin0001");
        let admin2 = HumanAddr::from("admin0002");
        let admin3 = HumanAddr::from("admin0003");
        let initial_admins = vec![owner.clone(), admin2.clone()];

        let env = mock_env(owner.clone(), &[]);
        setup_test_case(&mut deps, &env, &initial_admins, &vec![], &vec![], &vec![]);

        // Verify
        let config = query_admin_list(&deps).unwrap();
        assert_eq!(
            config,
            AdminListResponse {
                admins: initial_admins.clone(),
                mutable: true,
            }
        );

        // Add a third (new) admin
        let new_admins = vec![owner.clone(), admin2.clone(), admin3.clone()];
        let msg = HandleMsg::UpdateAdmins {
            admins: new_admins.clone(),
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let config = query_admin_list(&deps).unwrap();
        println!("config: {:#?}", config);
        assert_eq!(
            config,
            AdminListResponse {
                admins: new_admins,
                mutable: true,
            }
        );

        // Set admin3 as the only admin
        let msg = HandleMsg::UpdateAdmins {
            admins: vec![admin3.clone()],
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify admin3 is now the sole admin
        let config = query_admin_list(&deps).unwrap();
        println!("config: {:#?}", config);
        assert_eq!(
            config,
            AdminListResponse {
                admins: vec![admin3.clone()],
                mutable: true,
            }
        );

        // Try to add owner back
        let msg = HandleMsg::UpdateAdmins {
            admins: vec![admin3.clone(), owner.clone()],
        };
        let res = handle(&mut deps, env.clone(), msg);

        // Verify it fails (admin3 is now the owner)
        assert!(res.is_err());

        // Connect as admin3
        let env = mock_env(admin3.clone(), &[]);
        // Add owner back
        let msg = HandleMsg::UpdateAdmins {
            admins: vec![admin3.clone(), owner.clone()],
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let config = query_admin_list(&deps).unwrap();
        println!("config: {:#?}", config);
        assert_eq!(
            config,
            AdminListResponse {
                admins: vec![admin3, owner],
                mutable: true,
            }
        );
    }

    #[test]
    fn increase_allowances() {
        let mut deps = mock_dependencies(20, &coins(1111, "token1"));

        let owner = HumanAddr::from("admin0001");
        let admins = vec![owner.clone(), HumanAddr::from("admin0002")];

        let spender1 = HumanAddr::from("spender0001");
        let spender2 = HumanAddr::from("spender0002");
        let spender3 = HumanAddr::from("spender0003");
        let spender4 = HumanAddr::from("spender0004");
        let initial_spenders = vec![spender1.clone(), spender2.clone()];

        // Same allowances for all spenders, for simplicity
        let denom1 = "token1";
        let denom2 = "token2";
        let denom3 = "token3";
        let amount1 = 1111;
        let amount2 = 2222;
        let amount3 = 3333;

        let allow1 = coin(amount1, denom1);
        let allow2 = coin(amount2, denom2);
        let allow3 = coin(amount3, denom3);
        let initial_allowances = vec![allow1.clone(), allow2.clone()];

        let expires_height = Expiration::AtHeight { height: 5432 };
        let expires_never = Expiration::Never {};
        let expires_time = Expiration::AtTime { time: 1234567890 };
        // Initially set first spender allowance with height expiration, the second with no expiration
        let initial_expirations = vec![expires_height.clone(), expires_never.clone()];

        let env = mock_env(owner, &[]);
        setup_test_case(
            &mut deps,
            &env,
            &admins,
            &initial_spenders,
            &initial_allowances,
            &initial_expirations,
        );

        // Add to spender1 account (expires = None) => don't change Expiration
        let msg = HandleMsg::IncreaseAllowance {
            spender: spender1.clone(),
            amount: allow1.clone(),
            expires: None,
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender1.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![coin(amount1 * 2, &allow1.denom), allow2.clone()]),
                expires: expires_height.clone()
            }
        );

        // Add to spender2 account (expires = Some)
        let msg = HandleMsg::IncreaseAllowance {
            spender: spender2.clone(),
            amount: allow3.clone(),
            expires: Some(expires_height.clone()),
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender2.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow1.clone(), allow2.clone(), allow3.clone()]),
                expires: expires_height.clone()
            }
        );

        // Add to spender3 (new account) (expires = None) => default Expiration::Never
        let msg = HandleMsg::IncreaseAllowance {
            spender: spender3.clone(),
            amount: allow1.clone(),
            expires: None,
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender3.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow1.clone()]),
                expires: expires_never.clone()
            }
        );

        // Add to spender4 (new account) (expires = Some)
        let msg = HandleMsg::IncreaseAllowance {
            spender: spender4.clone(),
            amount: allow2.clone(),
            expires: Some(expires_time.clone()),
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender4.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow2.clone()]),
                expires: expires_time,
            }
        );
    }

    #[test]
    fn decrease_allowances() {
        let mut deps = mock_dependencies(20, &coins(1111, "token1"));

        let owner = HumanAddr::from("admin0001");
        let admins = vec![owner.clone(), HumanAddr::from("admin0002")];

        let spender1 = HumanAddr::from("spender0001");
        let spender2 = HumanAddr::from("spender0002");
        let initial_spenders = vec![spender1.clone(), spender2.clone()];

        // Same allowances for all spenders, for simplicity
        let denom1 = "token1";
        let denom2 = "token2";
        let denom3 = "token3";
        let amount1 = 1111;
        let amount2 = 2222;
        let amount3 = 3333;

        let allow1 = coin(amount1, denom1);
        let allow2 = coin(amount2, denom2);
        let allow3 = coin(amount3, denom3);

        let initial_allowances = vec![coin(amount1, denom1), coin(amount2, denom2)];

        let expires_height = Expiration::AtHeight { height: 5432 };
        let expires_never = Expiration::Never {};
        // Initially set first spender allowance with height expiration, the second with no expiration
        let initial_expirations = vec![expires_height.clone(), expires_never.clone()];

        let env = mock_env(owner, &[]);
        setup_test_case(
            &mut deps,
            &env,
            &admins,
            &initial_spenders,
            &initial_allowances,
            &initial_expirations,
        );

        // Subtract from spender1 (existing) account (has none of that denom)
        let msg = HandleMsg::DecreaseAllowance {
            spender: spender1.clone(),
            amount: allow3.clone(),
            expires: None,
        };
        let res = handle(&mut deps, env.clone(), msg);

        // Verify
        assert!(res.is_err());
        // Verify everything stays the same for that spender
        let allowance = query_allowance(&deps, spender1.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow1.clone(), allow2.clone()]),
                expires: expires_height.clone()
            }
        );

        // Subtract from spender2 (existing) account (brings denom to 0, other denoms left)
        let msg = HandleMsg::DecreaseAllowance {
            spender: spender2.clone(),
            amount: allow2.clone(),
            expires: None,
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender2.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow1.clone()]),
                expires: expires_never.clone()
            }
        );

        // Subtract from spender1 (existing) account (brings denom to > 0)
        let msg = HandleMsg::DecreaseAllowance {
            spender: spender1.clone(),
            amount: coin(amount1 / 2, denom1),
            expires: None,
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender1.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![
                    coin(amount1 / 2 + (amount1 & 1), denom1),
                    allow2.clone()
                ]),
                expires: expires_height.clone()
            }
        );

        // Subtract from spender2 (existing) account (brings denom to 0, no other denoms left => should delete Allowance)
        let msg = HandleMsg::DecreaseAllowance {
            spender: spender2.clone(),
            amount: allow1.clone(),
            expires: None,
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender2.clone()).unwrap();
        assert_eq!(allowance, Allowance::default());

        // Subtract from spender2 (empty) account (should error)
        let msg = HandleMsg::DecreaseAllowance {
            spender: spender2.clone(),
            amount: allow1.clone(),
            expires: None,
        };
        let res = handle(&mut deps, env.clone(), msg);

        // Verify
        assert!(res.is_err());

        // Subtract from spender1 (existing) account (underflows denom => should delete denom)
        let msg = HandleMsg::DecreaseAllowance {
            spender: spender1.clone(),
            amount: coin(amount1 * 10, denom1),
            expires: None,
        };
        handle(&mut deps, env.clone(), msg).unwrap();

        // Verify
        let allowance = query_allowance(&deps, spender1.clone()).unwrap();
        assert_eq!(
            allowance,
            Allowance {
                balance: Balance(vec![allow2]),
                expires: expires_height.clone()
            }
        );
    }

    #[test]
    fn execute_checks() {
        let mut deps = mock_dependencies(20, &coins(1111, "token1"));

        let owner = HumanAddr::from("admin0001");
        let admins = vec![owner.clone(), HumanAddr::from("admin0002")];

        let spender1 = HumanAddr::from("spender0001");
        let spender2 = HumanAddr::from("spender0002");
        let initial_spenders = vec![spender1.clone()];

        let denom1 = "token1";
        let amount1 = 1111;
        let allow1 = coin(amount1, denom1);
        let initial_allowances = vec![allow1];

        let expires_never = Expiration::Never {};
        let initial_expirations = vec![expires_never.clone()];

        let env = mock_env(owner.clone(), &[]);
        setup_test_case(
            &mut deps,
            &env,
            &admins,
            &initial_spenders,
            &initial_allowances,
            &initial_expirations,
        );

        // Create Send message
        let msgs = vec![BankMsg::Send {
            from_address: HumanAddr::from(MOCK_CONTRACT_ADDR),
            to_address: spender2.clone(),
            amount: coins(1000, "token1"),
        }
        .into()];

        let handle_msg = HandleMsg::Execute { msgs: msgs.clone() };

        // spender2 cannot spend funds (no initial allowance)
        let env = mock_env(&spender2, &[]);
        let res = handle(&mut deps, env, handle_msg.clone());
        match res.unwrap_err() {
            StdError::NotFound { .. } => {}
            e => panic!("unexpected error: {}", e),
        }

        // But spender1 can (he has enough funds)
        let env = mock_env(&spender1, &[]);
        let res = handle(&mut deps, env.clone(), handle_msg.clone()).unwrap();
        assert_eq!(res.messages, msgs);
        assert_eq!(
            res.log,
            vec![log("action", "execute"), log("owner", spender1.clone())]
        );

        // And then cannot (not enough funds anymore)
        let res = handle(&mut deps, env, handle_msg.clone());
        match res.unwrap_err() {
            StdError::Underflow { .. } => {}
            e => panic!("unexpected error: {}", e),
        }

        // Owner / admins can do anything (at the contract level)
        let env = mock_env(&owner.clone(), &[]);
        let res = handle(&mut deps, env.clone(), handle_msg.clone()).unwrap();
        assert_eq!(res.messages, msgs);
        assert_eq!(
            res.log,
            vec![log("action", "execute"), log("owner", owner.clone())]
        );

        // For admins, even other message types are allowed
        let other_msgs = vec![CosmosMsg::Custom(Empty {})];
        let handle_msg = HandleMsg::Execute {
            msgs: other_msgs.clone(),
        };

        let env = mock_env(&owner, &[]);
        let res = handle(&mut deps, env, handle_msg.clone()).unwrap();
        assert_eq!(res.messages, other_msgs);
        assert_eq!(res.log, vec![log("action", "execute"), log("owner", owner)]);

        // But not for mere mortals
        let env = mock_env(&spender1, &[]);
        let res = handle(&mut deps, env, handle_msg.clone());
        match res.unwrap_err() {
            StdError::GenericErr { msg, .. } => assert_eq!(&msg, "Message type rejected"),
            e => panic!("unexpected error: {}", e),
        }
    }
}
