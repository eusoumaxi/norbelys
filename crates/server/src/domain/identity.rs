//! Identity decisions: the pure rules of sign-in, sessions, single sign-on and membership,
//! separate from how they are stored or served.
//!
//! - **Sessions** ([`session_lifetime`], [`session_standing`], [`touch_due`]). A browser session
//!   lives 30 days without use and 90 days at most. A session proven through a workspace's single
//!   sign-on, or a break-glass session, lives 24 hours at most, so a person removed at the identity
//!   provider loses access within a day even without any provisioning protocol; an operator's
//!   impersonation lives 10 minutes. The idle clock
//!   moves at most once every 5 minutes, so a busy dashboard does not write its session row on
//!   every call.
//! - **External identities** ([`resolve_external`]). An external account (an OpenID Connect issuer
//!   and subject) already linked to a user signs that user in. An unlinked one creates a user only
//!   from an email address the provider marks verified and that no user holds yet. A verified
//!   address that a user already holds is never merged automatically: whoever controls an
//!   identity provider could otherwise take over any account by asserting its address. The person
//!   signs in their usual way and links the identity explicitly.
//! - **Just-in-time membership** ([`admit_by_sso`]). Single sign-on creates a membership with the
//!   connection's default role only when just-in-time provisioning is on, the email's domain is one
//!   the connection proved, and the person has no membership row at all: a suspended or removed
//!   membership is a tombstone that signing in never revives.
//! - **Enforcement** ([`proof_holds`]). In a workspace that enforces single sign-on, a credential
//!   step (minting a workspace token) needs a session proven through that workspace's connection,
//!   under the connection's current policy version, by an identity-provider authentication younger
//!   than 24 hours. Editing the connection bumps its policy version, so every older proof stops
//!   counting at once. An owner locked out by a broken identity provider gets a break-glass
//!   session from an operator: it stands in for the enforcing connection in that workspace alone,
//!   and its tokens repair the workspace without reaching its product data ([`token_scopes`]).
//! - **Membership changes** ([`membership_change`]). Only an owner touches an owner (promotes to
//!   owner, demotes, suspends or removes one), and the last active owner can never be demoted,
//!   suspended or removed, so a workspace always keeps someone who can govern it.
//! - **Slugs** ([`slug_from_name`]). A workspace created without a slug gets one derived from its
//!   name.
//!
//! Nothing here reads the clock: every decision takes `now` as an argument.

use std::time::Duration;

use uuid::Uuid;

use crate::domain::scope::{MembershipRole, Scope, ScopeSet};

/// How long a session may go unused.
pub const SESSION_IDLE: Duration = Duration::from_secs(30 * 24 * 3600);
/// The longest a session lives, however often it is used.
pub const SESSION_ABSOLUTE: Duration = Duration::from_secs(90 * 24 * 3600);
/// The longest a session proven through single sign-on (or break-glass) lives.
pub const SESSION_SHORT: Duration = Duration::from_secs(24 * 3600);
/// The longest an operator's impersonation of a person lives.
pub const SESSION_IMPERSONATION: Duration = Duration::from_secs(10 * 60);
/// How often a session's idle expiry is pushed forward at most.
pub const TOUCH_EVERY: Duration = Duration::from_secs(5 * 60);
/// How old an identity provider's authentication may be for an enforcing workspace.
pub const PROOF_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// Clock skew tolerated between the database, identity providers and serving replicas. A proof
/// slightly ahead of this process is usable immediately; its 24-hour expiry is never extended.
pub const CLOCK_SKEW: Duration = Duration::from_secs(10);

/// How a session authenticated (`sessions.auth_method`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// A one-time code or link sent to the user's email address.
    EmailCode,
    /// A passkey (WebAuthn).
    Passkey,
    /// An OpenID Connect provider offered to everyone (Google).
    Oidc,
    /// A workspace's own identity provider, routed by email domain.
    Sso,
    /// An audited owner recovery session under enforced single sign-on.
    BreakGlass,
    /// An operator acting as the person for support, for 10 minutes, audited.
    Impersonation,
}

impl AuthMethod {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// How long a new session lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lifetime {
    /// From creation, whatever the use.
    pub absolute: Duration,
    /// Without use; never past the absolute expiry.
    pub idle: Duration,
}

/// The lifetime of a session authenticated by `method`.
#[must_use]
pub fn session_lifetime(method: AuthMethod) -> Lifetime {
    match method {
        AuthMethod::EmailCode | AuthMethod::Passkey | AuthMethod::Oidc => Lifetime {
            absolute: SESSION_ABSOLUTE,
            idle: SESSION_IDLE,
        },
        AuthMethod::Sso | AuthMethod::BreakGlass => Lifetime {
            absolute: SESSION_SHORT,
            idle: SESSION_SHORT,
        },
        AuthMethod::Impersonation => Lifetime {
            absolute: SESSION_IMPERSONATION,
            idle: SESSION_IMPERSONATION,
        },
    }
}

/// What a session row says about its validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionClock {
    /// When it was revoked (signed out, revoked by its user, its credential lost), if it was.
    pub revoked_at: Option<jiff::Timestamp>,
    /// Its absolute expiry.
    pub expires_at: jiff::Timestamp,
    /// Its idle expiry.
    pub idle_expires_at: jiff::Timestamp,
}

/// Whether a session still proves its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Standing {
    /// It proves its user.
    Active,
    /// It was revoked.
    Revoked,
    /// Its absolute lifetime ended.
    Expired,
    /// It went unused for too long.
    Idle,
}

/// The standing of a session at `now`. Revocation wins over expiry, and the absolute expiry over
/// the idle one, so the reason recorded is the most deliberate one. A revoked session is revoked
/// whatever its `revoked_at` says against `now`: the database stamps it with its own clock, which
/// may run ahead of this process's, and a revocation must never wait for clocks to agree.
#[must_use]
pub fn session_standing(clock: &SessionClock, now: jiff::Timestamp) -> Standing {
    if clock.revoked_at.is_some() {
        Standing::Revoked
    } else if clock.expires_at <= now {
        Standing::Expired
    } else if clock.idle_expires_at <= now {
        Standing::Idle
    } else {
        Standing::Active
    }
}

/// Whether a session last seen at `last_seen_at` should have its idle expiry pushed forward now.
#[must_use]
pub fn touch_due(last_seen_at: jiff::Timestamp, now: jiff::Timestamp) -> bool {
    let elapsed = now.duration_since(last_seen_at);
    !elapsed.is_negative() && elapsed.unsigned_abs() >= TOUCH_EVERY
}

/// The idle expiry of a session used at `now`: `now` plus its idle lifetime, never past its
/// absolute expiry.
#[must_use]
pub fn idle_expiry(
    method: AuthMethod,
    now: jiff::Timestamp,
    expires_at: jiff::Timestamp,
) -> jiff::Timestamp {
    let idle = jiff::SignedDuration::try_from(session_lifetime(method).idle)
        .unwrap_or(jiff::SignedDuration::MAX);
    now.saturating_add(idle)
        .unwrap_or(jiff::Timestamp::MAX)
        .min(expires_at)
}

/// What is known when an external identity comes back from its provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalFacts {
    /// The user the (issuer, subject) pair is linked to, if any.
    pub linked_user: Option<Uuid>,
    /// Whether the provider gave an email address and marked it verified.
    pub email_verified: bool,
    /// The user who already holds that email address, if any.
    pub email_holder: Option<Uuid>,
}

/// What a returning external identity resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Sign in this user.
    SignIn(Uuid),
    /// Create a user with the verified address, linked to the identity.
    CreateUser,
    /// Refuse: the provider vouched for no email address, so no account can be created from it.
    Unverified,
    /// Refuse: a user already holds the address; they sign in their usual way and link the
    /// identity explicitly.
    NeedsLink,
}

/// Resolves a returning external identity (see the module).
#[must_use]
pub fn resolve_external(facts: ExternalFacts) -> Resolution {
    match facts {
        ExternalFacts {
            linked_user: Some(user),
            ..
        } => Resolution::SignIn(user),
        ExternalFacts {
            email_verified: false,
            ..
        } => Resolution::Unverified,
        ExternalFacts {
            email_holder: Some(_),
            ..
        } => Resolution::NeedsLink,
        ExternalFacts { .. } => Resolution::CreateUser,
    }
}

/// A membership's status (`memberships.status`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    /// The member acts in the workspace.
    Active,
    /// The member is paused: no access, keys revoked, the row kept.
    Suspended,
    /// The member left or was removed: a tombstone, never revived by signing in.
    Removed,
}

impl MembershipStatus {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// What single sign-on does about a person's membership in the connection's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// They are an active member already.
    Member,
    /// Create a membership with the connection's default role.
    Create,
    /// No membership: a suspended or removed one is a tombstone that signing in never revives.
    Tombstone,
    /// No membership: just-in-time provisioning is off (an invitation is needed).
    NotProvisioned,
    /// No membership: the email's domain is not one the connection proved.
    ForeignDomain,
}

/// Decides a person's membership after a single sign-on (see the module).
#[must_use]
pub fn admit_by_sso(
    existing: Option<MembershipStatus>,
    jit_provisioning: bool,
    domain_proved: bool,
) -> Admission {
    match existing {
        Some(MembershipStatus::Active) => Admission::Member,
        Some(MembershipStatus::Suspended | MembershipStatus::Removed) => Admission::Tombstone,
        None if !jit_provisioning => Admission::NotProvisioned,
        None if !domain_proved => Admission::ForeignDomain,
        None => Admission::Create,
    }
}

/// A workspace's enforced single sign-on: the connection a proof must come through, and its
/// current policy version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enforcement {
    /// The enforcing connection.
    pub connection: Uuid,
    /// Its current policy version.
    pub policy_version: i32,
}

/// How a session (or a grant) proved itself: its workspace authentication proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proof {
    /// The method.
    pub method: AuthMethod,
    /// The SSO connection, for `sso`.
    pub connection: Option<Uuid>,
    /// The connection's policy version at the proof, for `sso`.
    pub policy_version: Option<i32>,
    /// When the person authenticated: the identity provider's `auth_time` for `sso`.
    pub authenticated_at: jiff::Timestamp,
}

/// Why a proof does not satisfy an enforcing workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ProofRefusal {
    /// The session did not authenticate through the workspace's connection.
    NotThroughConnection,
    /// The connection's policy changed since the proof.
    PolicyChanged,
    /// The identity provider's authentication is older than 24 hours.
    Stale,
}

/// Whether `proof` satisfies a workspace whose enforcing connections are `enforcing` at `now`: a
/// workspace that enforces nothing accepts every proof; otherwise the proof must come through one
/// of them, under its current policy version, from an authentication younger than 24 hours.
///
/// A break-glass session stands in for the enforcing connection it was opened for: it holds in
/// that connection's workspace while the connection enforces, under any policy version (the repair
/// itself edits the connection), for 24 hours, and nowhere else, not even in a workspace that
/// enforces nothing.
///
/// # Errors
///
/// The reason the proof does not count.
pub fn proof_holds(
    enforcing: &[Enforcement],
    proof: &Proof,
    now: jiff::Timestamp,
) -> Result<(), ProofRefusal> {
    let fresh = || {
        let age = now.duration_since(proof.authenticated_at);
        if (age.is_negative() && age.unsigned_abs() > CLOCK_SKEW)
            || (!age.is_negative() && age.unsigned_abs() >= PROOF_MAX_AGE)
        {
            Err(ProofRefusal::Stale)
        } else {
            Ok(())
        }
    };
    if proof.method == AuthMethod::BreakGlass {
        let stands_in = enforcing
            .iter()
            .any(|enforcement| proof.connection == Some(enforcement.connection));
        return if stands_in {
            fresh()
        } else {
            Err(ProofRefusal::NotThroughConnection)
        };
    }
    if enforcing.is_empty() {
        return Ok(());
    }
    let through = enforcing.iter().find(|enforcement| {
        proof.method == AuthMethod::Sso && proof.connection == Some(enforcement.connection)
    });
    let Some(enforcement) = through else {
        return Err(ProofRefusal::NotThroughConnection);
    };
    if proof.policy_version != Some(enforcement.policy_version) {
        return Err(ProofRefusal::PolicyChanged);
    }
    fresh()
}

/// The scopes a workspace token minted from a session authenticated by `method` carries in a
/// workspace where the person holds `role`: the role's, except a break-glass session's, which
/// repair the workspace (read it, manage its settings, members and single sign-on) and never
/// reach its product data.
#[must_use]
pub fn token_scopes(method: AuthMethod, role: MembershipRole) -> ScopeSet {
    match method {
        AuthMethod::BreakGlass => role.scopes().intersect(
            [Scope::WorkspaceRead, Scope::WorkspaceManage]
                .into_iter()
                .collect(),
        ),
        AuthMethod::EmailCode
        | AuthMethod::Passkey
        | AuthMethod::Oidc
        | AuthMethod::Sso
        | AuthMethod::Impersonation => role.scopes(),
    }
}

/// A credential a session can make that outlives the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Lasting {
    /// An OAuth grant for an application, by consent or a device approval: 90 days.
    Grant,
    /// An API key: until it is revoked.
    ApiKey,
    /// Recovery codes: until one is used or the set is replaced.
    RecoveryCodes,
    /// A passkey: until it is removed.
    Passkey,
    /// A linked external identity, a way to sign in: until it is unlinked.
    IdentityLink,
}

/// Whether a session authenticated by `method` may make `credential`.
///
/// An operator's impersonation acts in the person's name for ten minutes, under audit, so that
/// support can see what the person sees; anything lasting it made would carry the person's access
/// past the session, in the person's name and out of the operator's audit trail, so it makes
/// none. A break-glass session is the owner themselves, verified by an operator and a recovery
/// code: what belongs to their own account (keys bounded by the repair scopes, codes, passkeys,
/// identities) stays theirs to make, but no application is granted access from a recovery.
#[must_use]
pub fn may_make(method: AuthMethod, credential: Lasting) -> bool {
    match method {
        AuthMethod::EmailCode | AuthMethod::Passkey | AuthMethod::Oidc | AuthMethod::Sso => true,
        AuthMethod::BreakGlass => credential != Lasting::Grant,
        AuthMethod::Impersonation => false,
    }
}

/// A change to a membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChange {
    /// Give the member this role (making someone `owner` is how ownership is transferred).
    Role(MembershipRole),
    /// Pause the member.
    Suspend,
    /// Let a suspended member act again.
    Reactivate,
    /// Remove the member (a tombstone).
    Remove,
}

/// What a membership change is decided on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeFacts {
    /// The role of the member making the change.
    pub actor_role: MembershipRole,
    /// The target's current role.
    pub target_role: MembershipRole,
    /// The target's current status.
    pub target_status: MembershipStatus,
    /// Active owners of the workspace, the target included when it is one.
    pub active_owners: i64,
}

/// Why a membership change is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ChangeRefusal {
    /// Only an owner promotes to owner or changes an owner.
    OwnersOnly,
    /// The change would leave the workspace without an active owner.
    LastOwner,
    /// A removed membership is a tombstone; an invitation brings the person back.
    Removed,
}

/// Decides a membership change (see the module).
///
/// # Errors
///
/// The reason the change is refused.
pub fn membership_change(
    change: MembershipChange,
    facts: &ChangeFacts,
) -> Result<(), ChangeRefusal> {
    if facts.target_status == MembershipStatus::Removed {
        return Err(ChangeRefusal::Removed);
    }
    let touches_owner = facts.target_role == MembershipRole::Owner
        || change == MembershipChange::Role(MembershipRole::Owner);
    if touches_owner && facts.actor_role != MembershipRole::Owner {
        return Err(ChangeRefusal::OwnersOnly);
    }
    let loses_owner = facts.target_role == MembershipRole::Owner
        && facts.target_status == MembershipStatus::Active
        && match change {
            MembershipChange::Role(role) => role != MembershipRole::Owner,
            MembershipChange::Suspend | MembershipChange::Remove => true,
            MembershipChange::Reactivate => false,
        };
    if loses_owner && facts.active_owners <= 1 {
        return Err(ChangeRefusal::LastOwner);
    }
    Ok(())
}

/// A slug derived from a workspace's name: ASCII lowercase letters and digits, runs of anything
/// else turned into one hyphen, no hyphen at either end, at most 40 characters; `workspace` when
/// nothing usable is left. The caller appends a random suffix, which keeps the result within the
/// slug's 63 characters and makes it unique in practice.
#[must_use]
pub fn slug_from_name(name: &str) -> String {
    let mut slug = String::with_capacity(name.len().min(40));
    let mut pending_hyphen = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_hyphen && !slug.is_empty() {
                slug.push('-');
            }
            pending_hyphen = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.len() < 2 {
        "workspace".to_owned()
    } else {
        slug.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;

    fn at(seconds: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_790_000_000 + seconds).unwrap()
    }

    /// Every sign-in method has a lifetime, and only the methods a workspace's identity provider
    /// (or an owner's recovery) stands behind are cut to one day, so deprovisioning at the provider
    /// is honoured within a day while ordinary sessions keep their 30 idle and 90 absolute days;
    /// an operator's impersonation ends after 10 minutes, used or not.
    #[test]
    fn each_method_has_its_session_lifetime() {
        for method in AuthMethod::iter() {
            let expected = match method {
                AuthMethod::EmailCode | AuthMethod::Passkey | AuthMethod::Oidc => {
                    (SESSION_ABSOLUTE, SESSION_IDLE)
                }
                AuthMethod::Sso | AuthMethod::BreakGlass => (SESSION_SHORT, SESSION_SHORT),
                AuthMethod::Impersonation => (SESSION_IMPERSONATION, SESSION_IMPERSONATION),
            };
            let lifetime = session_lifetime(method);
            assert_eq!((lifetime.absolute, lifetime.idle), expected, "{method:?}");
            assert!(lifetime.idle <= lifetime.absolute, "{method:?}");
        }
    }

    /// Each standing is reached by exactly the clock that says so, and revocation is reported over
    /// expiry and absolute expiry over idleness, so the reason a session stopped is the most
    /// deliberate one.
    #[test]
    fn a_session_stands_until_its_first_ending() {
        let now = at(0);
        for standing in Standing::iter() {
            let clock = match standing {
                Standing::Active => SessionClock {
                    revoked_at: None,
                    expires_at: at(10),
                    idle_expires_at: at(5),
                },
                // Stamped by a database clock running ahead of ours: revoked all the same.
                Standing::Revoked => SessionClock {
                    revoked_at: Some(at(5)),
                    expires_at: at(-5),
                    idle_expires_at: at(-5),
                },
                Standing::Expired => SessionClock {
                    revoked_at: None,
                    expires_at: at(0),
                    idle_expires_at: at(-5),
                },
                Standing::Idle => SessionClock {
                    revoked_at: None,
                    expires_at: at(10),
                    idle_expires_at: at(0),
                },
            };
            assert_eq!(session_standing(&clock, now), standing);
        }
    }

    /// The idle expiry moves at most every five minutes and never past the absolute expiry, so a
    /// busy dashboard writes its session row rarely and an idle extension never outlives the
    /// session.
    #[test]
    fn the_idle_clock_moves_rarely_and_stays_inside_the_absolute_expiry() {
        assert!(!touch_due(at(0), at(299)));
        assert!(touch_due(at(0), at(300)));
        assert!(
            !touch_due(at(10), at(0)),
            "a clock moving backwards never touches"
        );
        assert_eq!(
            idle_expiry(AuthMethod::EmailCode, at(0), at(3600)),
            at(3600),
            "capped by the absolute expiry"
        );
        assert_eq!(
            idle_expiry(AuthMethod::Passkey, at(0), at(100 * 24 * 3600)),
            at(30 * 24 * 3600)
        );
    }

    /// Every combination of what a provider tells resolves as the module says: a link always wins,
    /// an unverified address never creates an account, and a verified address someone already holds
    /// is never merged without the person linking it.
    #[test]
    fn external_identities_resolve_without_silent_merges() {
        let (linked, holder) = (Uuid::from_u128(1), Uuid::from_u128(2));
        for linked_user in [None, Some(linked)] {
            for email_verified in [false, true] {
                for email_holder in [None, Some(holder)] {
                    let facts = ExternalFacts {
                        linked_user,
                        email_verified,
                        email_holder,
                    };
                    let expected = match (linked_user, email_verified, email_holder) {
                        (Some(user), _, _) => Resolution::SignIn(user),
                        (None, false, _) => Resolution::Unverified,
                        (None, true, Some(_)) => Resolution::NeedsLink,
                        (None, true, None) => Resolution::CreateUser,
                    };
                    assert_eq!(resolve_external(facts), expected, "{facts:?}");
                }
            }
        }
    }

    /// Just-in-time provisioning creates a membership only for a person without any membership
    /// row, with provisioning on and a proved domain; a tombstone is never revived.
    #[test]
    fn single_sign_on_admits_only_new_people_of_proved_domains() {
        let statuses = std::iter::once(None).chain(MembershipStatus::iter().map(Some));
        for existing in statuses {
            for jit in [false, true] {
                for domain in [false, true] {
                    let expected = match existing {
                        Some(MembershipStatus::Active) => Admission::Member,
                        Some(MembershipStatus::Suspended | MembershipStatus::Removed) => {
                            Admission::Tombstone
                        }
                        None if !jit => Admission::NotProvisioned,
                        None if !domain => Admission::ForeignDomain,
                        None => Admission::Create,
                    };
                    assert_eq!(
                        admit_by_sso(existing, jit, domain),
                        expected,
                        "{existing:?} jit={jit} domain={domain}"
                    );
                }
            }
        }
    }

    /// An enforcing workspace accepts only a fresh proof through its own connection under its
    /// current policy, whatever the method; a workspace that enforces nothing accepts every proof.
    /// A break-glass session holds only where it stands in for an enforcing connection, under any
    /// policy version but for no more than a day, and nowhere else.
    #[test]
    fn enforcement_accepts_only_a_fresh_proof_through_its_connection() {
        let (connection, other) = (Uuid::from_u128(7), Uuid::from_u128(8));
        let enforcement = Enforcement {
            connection,
            policy_version: 3,
        };
        let now = at(0);
        for method in AuthMethod::iter() {
            let proof = Proof {
                method,
                connection: Some(connection),
                policy_version: Some(3),
                authenticated_at: at(-60),
            };
            // What the proof is worth where nothing is enforced, and under the enforcement.
            let expected = match method {
                AuthMethod::Sso => (Ok(()), Ok(())),
                AuthMethod::BreakGlass => (Err(ProofRefusal::NotThroughConnection), Ok(())),
                AuthMethod::EmailCode
                | AuthMethod::Passkey
                | AuthMethod::Oidc
                | AuthMethod::Impersonation => (Ok(()), Err(ProofRefusal::NotThroughConnection)),
            };
            assert_eq!(
                (
                    proof_holds(&[], &proof, now),
                    proof_holds(&[enforcement], &proof, now)
                ),
                expected,
                "{method:?}"
            );
        }
        let break_glass = |connection, policy_version, authenticated_at| Proof {
            method: AuthMethod::BreakGlass,
            connection: Some(connection),
            policy_version: Some(policy_version),
            authenticated_at,
        };
        let opened = [
            (
                break_glass(other, 3, at(-60)),
                Err(ProofRefusal::NotThroughConnection),
            ),
            (break_glass(connection, 2, at(-60)), Ok(())),
            (break_glass(connection, 3, at(10)), Ok(())),
            (break_glass(connection, 3, at(11)), Err(ProofRefusal::Stale)),
            (
                break_glass(connection, 3, at(-24 * 3600)),
                Err(ProofRefusal::Stale),
            ),
        ];
        for (proof, expected) in opened {
            assert_eq!(
                proof_holds(&[enforcement], &proof, now),
                expected,
                "{proof:?}"
            );
        }
        let sso = |connection, policy_version, authenticated_at| Proof {
            method: AuthMethod::Sso,
            connection: Some(connection),
            policy_version: Some(policy_version),
            authenticated_at,
        };
        let cases = [
            (
                sso(other, 3, at(-60)),
                Err(ProofRefusal::NotThroughConnection),
            ),
            (
                sso(connection, 2, at(-60)),
                Err(ProofRefusal::PolicyChanged),
            ),
            (sso(connection, 3, at(-24 * 3600)), Err(ProofRefusal::Stale)),
            (sso(connection, 3, at(60)), Err(ProofRefusal::Stale)),
            (sso(connection, 3, at(10)), Ok(())),
            (sso(connection, 3, at(11)), Err(ProofRefusal::Stale)),
            (sso(connection, 3, at(-24 * 3600 + 1)), Ok(())),
        ];
        for (proof, expected) in cases {
            assert_eq!(
                proof_holds(&[enforcement], &proof, now),
                expected,
                "{proof:?}"
            );
        }
    }

    /// A workspace token carries its person's role scopes, except one minted from a break-glass
    /// session, which only repairs the workspace (reads it, manages its settings, members and
    /// single sign-on) and never reaches product data, whatever the role.
    #[test]
    fn a_break_glass_token_only_repairs() {
        let repair: ScopeSet = [Scope::WorkspaceRead, Scope::WorkspaceManage]
            .into_iter()
            .collect();
        for method in AuthMethod::iter() {
            for role in MembershipRole::iter() {
                let expected = match method {
                    AuthMethod::BreakGlass => role.scopes().intersect(repair),
                    AuthMethod::EmailCode
                    | AuthMethod::Passkey
                    | AuthMethod::Oidc
                    | AuthMethod::Sso
                    | AuthMethod::Impersonation => role.scopes(),
                };
                assert_eq!(token_scopes(method, role), expected, "{method:?} {role:?}");
            }
        }
        let owner = token_scopes(AuthMethod::BreakGlass, MembershipRole::Owner);
        assert_eq!(owner, repair);
        assert!(!owner.contains(Scope::PeopleRead));
    }

    /// Every change by every role to every target, with one or two owners left: only an owner
    /// touches an owner, the last active owner never loses ownership, and a removed member is not
    /// edited back.
    #[test]
    fn membership_changes_keep_an_owner_and_owners_to_owners() {
        let changes = MembershipRole::iter()
            .map(MembershipChange::Role)
            .chain([
                MembershipChange::Suspend,
                MembershipChange::Reactivate,
                MembershipChange::Remove,
            ])
            .collect::<Vec<_>>();
        for change in &changes {
            for actor_role in MembershipRole::iter() {
                for target_role in MembershipRole::iter() {
                    for target_status in MembershipStatus::iter() {
                        for active_owners in [1, 2] {
                            let facts = ChangeFacts {
                                actor_role,
                                target_role,
                                target_status,
                                active_owners,
                            };
                            let touches_owner = target_role == MembershipRole::Owner
                                || *change == MembershipChange::Role(MembershipRole::Owner);
                            let demotes_active_owner = target_role == MembershipRole::Owner
                                && target_status == MembershipStatus::Active
                                && !matches!(
                                    change,
                                    MembershipChange::Role(MembershipRole::Owner)
                                        | MembershipChange::Reactivate
                                );
                            let expected = if target_status == MembershipStatus::Removed {
                                Err(ChangeRefusal::Removed)
                            } else if touches_owner && actor_role != MembershipRole::Owner {
                                Err(ChangeRefusal::OwnersOnly)
                            } else if demotes_active_owner && active_owners <= 1 {
                                Err(ChangeRefusal::LastOwner)
                            } else {
                                Ok(())
                            };
                            assert_eq!(
                                membership_change(*change, &facts),
                                expected,
                                "{change:?} {facts:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Names become readable slugs within bounds, and a name with nothing usable still gets one.
    #[test]
    fn slugs_come_from_names() {
        let cases = [
            ("Acme Inc.", "acme-inc"),
            ("  --Ünïcode & Co--  ", "n-code-co"),
            ("a", "workspace"),
            ("***", "workspace"),
            ("Growth Team 2026", "growth-team-2026"),
        ];
        for (name, expected) in cases {
            assert_eq!(slug_from_name(name), expected, "{name}");
        }
        let long = slug_from_name(&"x".repeat(100));
        assert_eq!(long.len(), 40);
    }

    /// Every sign-in method has a stated answer for every lasting credential: a person's own
    /// sign-in makes them all, a break-glass recovery all but an application's grant, and an
    /// operator's impersonation none, so nothing it made can outlast its ten audited minutes. A new
    /// method or credential fails here until it has an answer.
    #[test]
    fn only_the_persons_own_sign_in_makes_every_lasting_credential() {
        for method in AuthMethod::iter() {
            let allowed: Vec<Lasting> = match method {
                AuthMethod::EmailCode
                | AuthMethod::Passkey
                | AuthMethod::Oidc
                | AuthMethod::Sso => Lasting::iter().collect(),
                AuthMethod::BreakGlass => vec![
                    Lasting::ApiKey,
                    Lasting::RecoveryCodes,
                    Lasting::Passkey,
                    Lasting::IdentityLink,
                ],
                AuthMethod::Impersonation => Vec::new(),
            };
            for credential in Lasting::iter() {
                assert_eq!(
                    may_make(method, credential),
                    allowed.contains(&credential),
                    "{method:?} making {credential:?}"
                );
            }
        }
    }
}
