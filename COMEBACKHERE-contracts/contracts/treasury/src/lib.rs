#![no_std]

mod events;

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Vec};

#[contracttype]
pub enum SettlementStatus {
    Pending,
    Executed,
    PartiallyExecuted,
    OnHold,
    Cancelled,
}

#[contracttype]
pub struct Settlement {
    pub token: Address,
    pub amount: u64,
    pub merchant: Address,
    pub status: SettlementStatus,
    pub approval_weight: u64,
    pub proposer: Address,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisputeStatus {
    Raised,
    ResolvedClaimant,
    ResolvedCounterparty,
}

// Dispute records are stored on-chain so resolution votes are durable:
// they survive process restarts and are shared across backend replicas.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dispute {
    pub settlement_id: u64,
    pub status: DisputeStatus,
    /// Cumulative weight of the signers who have voted on the resolution.
    pub resolution_weight: u64,
    /// Signers who already voted, so each signer can only vote once.
    pub voters: Vec<Address>,
    pub raised_by: Address,
    pub reason: u32,
}

#[contract]
pub struct TreasuryContract;

#[contractimpl]
impl TreasuryContract {
    pub fn initialize(e: Env, signers: Vec<(Address, u64)>, threshold: u64, admin: Address) {
        admin.require_auth();
        e.storage().instance().set(&DataKey::Admin, &admin);
        e.storage().instance().set(&DataKey::Threshold, &threshold);
        e.storage().instance().set(&DataKey::Paused, &false);
        e.storage().instance().set(&DataKey::NextSettlementId, &1u64);
        for (signer, weight) in signers.iter() {
            e.storage().instance().set(&DataKey::Signer(signer.clone()), &weight);
        }
    }

    pub fn set_signer(e: Env, admin: Address, signer: Address, weight: u64) {
        admin.require_auth();
        e.storage().instance().set(&DataKey::Signer(signer), &weight);
    }

    pub fn propose_settlement(
        e: Env,
        signer: Address,
        token: Address,
        amount: u64,
        merchant: Address,
    ) -> u64 {
        signer.require_auth();

        if e.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
            panic_with_error!(&e, TreasuryError::ContractPaused);
        }

        let settlement_id = e
            .storage()
            .instance()
            .get(&DataKey::NextSettlementId)
            .unwrap_or(1u64);

        let settlement = Settlement {
            token: token.clone(),
            amount,
            merchant: merchant.clone(),
            status: SettlementStatus::Pending,
            approval_weight: 0u64,
            proposer: signer.clone(),
        };

        e.storage().instance().set(&DataKey::Settlement(settlement_id), &settlement);
        e.storage().instance().set(&DataKey::NextSettlementId, &(settlement_id + 1));

        settlement_id
    }

    pub fn approve_settlement(e: Env, signer: Address, settlement_id: u64) {
        signer.require_auth();
        let mut settlement = Self::get_settlement_internal(&e, settlement_id);
        if settlement.status != SettlementStatus::Pending {
            panic_with_error!(&e, TreasuryError::NotPending);
        }
        let weight: u64 = e
            .storage()
            .instance()
            .get(&DataKey::Signer(signer.clone()))
            .unwrap_or(0u64);
        settlement.approval_weight += weight;
        e.storage().instance().set(&DataKey::Settlement(settlement_id), &settlement);
    }

    pub fn execute_settlement(e: Env, signer: Address, settlement_id: u64, token_contract: Address) {
        signer.require_auth();
        let settlement = Self::get_settlement_internal(&e, settlement_id);
        if settlement.status != SettlementStatus::Pending {
            panic_with_error!(&e, TreasuryError::NotPending);
        }
        let threshold: u64 = e.storage().instance().get(&DataKey::Threshold).unwrap_or(0u64);
        if settlement.approval_weight < threshold {
            panic_with_error!(&e, TreasuryError::InsufficientApprovals);
        }
    }

    pub fn get_pending_settlements(e: Env) -> Vec<u64> {
        Vec::new(&e)
    }

    pub fn pause(e: Env, admin: Address) {
        admin.require_auth();
        e.storage().instance().set(&DataKey::Paused, &true);
    }

    pub fn unpause(e: Env, admin: Address) {
        admin.require_auth();
        e.storage().instance().set(&DataKey::Paused, &false);
    }

    pub fn raise_dispute(e: Env, signer: Address, settlement_id: u64, reason: u32) {
        signer.require_auth();

        if e.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
            panic_with_error!(&e, TreasuryError::ContractPaused);
        }

        if e.storage().instance().has(&DataKey::Dispute(settlement_id)) {
            panic_with_error!(&e, TreasuryError::DisputeAlreadyRaised);
        }

        let mut settlement = Self::get_settlement_internal(&e, settlement_id);
        settlement.status = SettlementStatus::OnHold;
        e.storage().instance().set(&DataKey::Settlement(settlement_id), &settlement);

        let dispute = Dispute {
            settlement_id,
            status: DisputeStatus::Raised,
            resolution_weight: 0u64,
            voters: Vec::new(&e),
            raised_by: signer.clone(),
            reason,
        };
        e.storage().instance().set(&DataKey::Dispute(settlement_id), &dispute);
        events::dispute_raised(&e, &settlement_id, &signer, &reason);
    }

    pub fn vote_dispute_resolution(
        e: Env,
        signer: Address,
        settlement_id: u64,
        resolve_in_favor: bool,
    ) {
        signer.require_auth();

        if e.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
            panic_with_error!(&e, TreasuryError::ContractPaused);
        }

        let mut dispute = Self::get_dispute_internal(&e, settlement_id);
        if dispute.status != DisputeStatus::Raised {
            panic_with_error!(&e, TreasuryError::DisputeNotRaised);
        }

        let weight: u64 = e
            .storage()
            .instance()
            .get(&DataKey::Signer(signer.clone()))
            .unwrap_or(0u64);
        if weight == 0 {
            panic_with_error!(&e, TreasuryError::UnauthorizedSigner);
        }

        if dispute.voters.iter().any(|voter| voter == signer) {
            panic_with_error!(&e, TreasuryError::AlreadyVoted);
        }

        dispute.voters.push(signer.clone());
        dispute.resolution_weight += weight;
        e.storage().instance().set(&DataKey::Dispute(settlement_id), &dispute);
        events::dispute_resolution_voted(
            &e,
            &settlement_id,
            &signer,
            &weight,
            &dispute.resolution_weight,
        );

        let threshold: u64 = e.storage().instance().get(&DataKey::Threshold).unwrap_or(0u64);
        if threshold > 0 && dispute.resolution_weight >= threshold {
            Self::finalize_dispute_internal(&e, settlement_id, resolve_in_favor);
        }
    }

    pub fn resolve_dispute(e: Env, signer: Address, settlement_id: u64, resolve_in_favor: bool) {
        signer.require_auth();

        if e.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
            panic_with_error!(&e, TreasuryError::ContractPaused);
        }

        let dispute = Self::get_dispute_internal(&e, settlement_id);
        if dispute.status != DisputeStatus::Raised {
            panic_with_error!(&e, TreasuryError::DisputeNotRaised);
        }

        let threshold: u64 = e.storage().instance().get(&DataKey::Threshold).unwrap_or(0u64);
        if dispute.resolution_weight < threshold {
            panic_with_error!(&e, TreasuryError::ThresholdNotMet);
        }

        Self::finalize_dispute_internal(&e, settlement_id, resolve_in_favor);
    }

    pub fn deposit(e: Env, from: Address, amount: u64) {
        from.require_auth();
    }

    pub fn withdraw(e: Env, admin: Address, to: Address, amount: u64) {
        admin.require_auth();
    }

    fn get_settlement_internal(e: &Env, settlement_id: u64) -> Settlement {
        e.storage()
            .instance()
            .get(&DataKey::Settlement(settlement_id))
            .unwrap()
    }

    fn get_dispute_internal(e: &Env, settlement_id: u64) -> Dispute {
        e.storage()
            .instance()
            .get(&DataKey::Dispute(settlement_id))
            .unwrap_or_else(|| panic_with_error!(e, TreasuryError::DisputeNotFound))
    }

    fn finalize_dispute_internal(e: &Env, settlement_id: u64, resolve_in_favor: bool) {
        let mut dispute: Dispute = e
            .storage()
            .instance()
            .get(&DataKey::Dispute(settlement_id))
            .unwrap_or_else(|| panic_with_error!(e, TreasuryError::DisputeNotFound));

        dispute.status = if resolve_in_favor {
            DisputeStatus::ResolvedClaimant
        } else {
            DisputeStatus::ResolvedCounterparty
        };
        e.storage().instance().set(&DataKey::Dispute(settlement_id), &dispute);

        // In favour of the claimant (the dispute raiser): the settlement is voided.
        // In favour of the counterparty (the merchant): the settlement resumes as
        // Pending and can proceed through the normal approval/execution flow.
        let mut settlement = Self::get_settlement_internal(e, settlement_id);
        settlement.status = if resolve_in_favor {
            SettlementStatus::Cancelled
        } else {
            SettlementStatus::Pending
        };
        e.storage().instance().set(&DataKey::Settlement(settlement_id), &settlement);

        events::dispute_resolved(e, &settlement_id, &resolve_in_favor, &dispute.resolution_weight);
    }
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    ContractPaused = 1,
    NotPending = 2,
    InsufficientApprovals = 3,
    DisputeNotFound = 4,
    DisputeAlreadyRaised = 5,
    DisputeNotRaised = 6,
    AlreadyVoted = 7,
    UnauthorizedSigner = 8,
    ThresholdNotMet = 9,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Paused,
    Signer(Address),
    Settlement(u64),
    Dispute(u64),
    NextSettlementId,
    Threshold,
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{vec, Env};

    struct TestContext {
        env: Env,
        contract_id: Address,
        signer1: Address,
        signer2: Address,
        signer3: Address,
        token: Address,
        merchant: Address,
    }

    fn setup() -> TestContext {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let signer1 = Address::generate(&env);
        let signer2 = Address::generate(&env);
        let signer3 = Address::generate(&env);
        let token = Address::generate(&env);
        let merchant = Address::generate(&env);

        let signers = vec![
            &env,
            (signer1.clone(), 1u64),
            (signer2.clone(), 1u64),
            (signer3.clone(), 1u64),
        ];

        let contract_id = env.register_contract(None, TreasuryContract);
        let client = TreasuryContractClient::new(&env, &contract_id);
        client.initialize(&signers, &2u64, &admin);

        TestContext {
            env,
            contract_id,
            signer1,
            signer2,
            signer3,
            token,
            merchant,
        }
    }

    fn propose_and_raise(ctx: &TestContext) -> u64 {
        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        let settlement_id = client.propose_settlement(&ctx.signer1, &ctx.token, &1000u64, &ctx.merchant);
        client.raise_dispute(&ctx.signer1, &settlement_id, &1u32);
        settlement_id
    }

    fn read_dispute(ctx: &TestContext, settlement_id: u64) -> Dispute {
        ctx.env.as_contract(&ctx.contract_id, || {
            ctx.env
                .storage()
                .instance()
                .get(&DataKey::Dispute(settlement_id))
                .unwrap()
        })
    }

    fn read_settlement(ctx: &TestContext, settlement_id: u64) -> Settlement {
        ctx.env.as_contract(&ctx.contract_id, || {
            ctx.env
                .storage()
                .instance()
                .get(&DataKey::Settlement(settlement_id))
                .unwrap()
        })
    }

    #[test]
    fn test_raise_dispute_holds_settlement_and_records_dispute() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let dispute = read_dispute(&ctx, settlement_id);
        assert_eq!(dispute.status, DisputeStatus::Raised);
        assert_eq!(dispute.resolution_weight, 0u64);
        assert_eq!(dispute.raised_by, ctx.signer1);
        assert_eq!(dispute.reason, 1u32);
        assert!(dispute.voters.is_empty());

        let settlement = read_settlement(&ctx, settlement_id);
        assert!(matches!(settlement.status, SettlementStatus::OnHold));
    }

    #[test]
    fn test_raise_dispute_twice_fails() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        let result = client.try_raise_dispute(&ctx.signer2, &settlement_id, &2u32);
        assert_eq!(result, Err(Ok(TreasuryError::DisputeAlreadyRaised)));
    }

    #[test]
    fn test_votes_resolve_dispute_in_favour_of_claimant() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);

        // First vote: weight 1 < threshold 2, dispute stays Raised.
        client.vote_dispute_resolution(&ctx.signer1, &settlement_id, &true);
        let dispute = read_dispute(&ctx, settlement_id);
        assert_eq!(dispute.status, DisputeStatus::Raised);
        assert_eq!(dispute.resolution_weight, 1u64);

        // Second vote reaches the threshold and resolves in favour of the claimant.
        client.vote_dispute_resolution(&ctx.signer2, &settlement_id, &true);
        let dispute = read_dispute(&ctx, settlement_id);
        assert_eq!(dispute.status, DisputeStatus::ResolvedClaimant);
        assert_eq!(dispute.resolution_weight, 2u64);

        let settlement = read_settlement(&ctx, settlement_id);
        assert!(matches!(settlement.status, SettlementStatus::Cancelled));
    }

    #[test]
    fn test_votes_resolve_dispute_in_favour_of_counterparty() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        client.vote_dispute_resolution(&ctx.signer1, &settlement_id, &false);
        client.vote_dispute_resolution(&ctx.signer2, &settlement_id, &false);

        let dispute = read_dispute(&ctx, settlement_id);
        assert_eq!(dispute.status, DisputeStatus::ResolvedCounterparty);

        let settlement = read_settlement(&ctx, settlement_id);
        assert!(matches!(settlement.status, SettlementStatus::Pending));
    }

    #[test]
    fn test_signer_cannot_vote_twice() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        client.vote_dispute_resolution(&ctx.signer1, &settlement_id, &true);

        let result = client.try_vote_dispute_resolution(&ctx.signer1, &settlement_id, &true);
        assert_eq!(result, Err(Ok(TreasuryError::AlreadyVoted)));
    }

    #[test]
    fn test_non_signer_cannot_vote() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        let outsider = Address::generate(&ctx.env);

        let result = client.try_vote_dispute_resolution(&outsider, &settlement_id, &true);
        assert_eq!(result, Err(Ok(TreasuryError::UnauthorizedSigner)));
    }

    #[test]
    fn test_vote_without_dispute_fails() {
        let ctx = setup();
        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        let settlement_id = client.propose_settlement(&ctx.signer1, &ctx.token, &1000u64, &ctx.merchant);

        let result = client.try_vote_dispute_resolution(&ctx.signer1, &settlement_id, &true);
        assert_eq!(result, Err(Ok(TreasuryError::DisputeNotFound)));
    }

    #[test]
    fn test_resolve_before_threshold_fails() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        client.vote_dispute_resolution(&ctx.signer1, &settlement_id, &true);

        let result = client.try_resolve_dispute(&ctx.signer2, &settlement_id, &true);
        assert_eq!(result, Err(Ok(TreasuryError::ThresholdNotMet)));
    }

    #[test]
    fn test_vote_after_resolution_fails() {
        let ctx = setup();
        let settlement_id = propose_and_raise(&ctx);

        let client = TreasuryContractClient::new(&ctx.env, &ctx.contract_id);
        client.vote_dispute_resolution(&ctx.signer1, &settlement_id, &true);
        client.vote_dispute_resolution(&ctx.signer2, &settlement_id, &true);

        let result = client.try_vote_dispute_resolution(&ctx.signer3, &settlement_id, &false);
        assert_eq!(result, Err(Ok(TreasuryError::DisputeNotRaised)));
    }
}
