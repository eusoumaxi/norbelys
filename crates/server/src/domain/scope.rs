//! Scopes and membership roles.
//!
//! A scope is `<area>:read` for listing and retrieving the area's resources, and the area's
//! write, manage or send scope for everything else. A member's role grants a fixed set of
//! scopes ([`MembershipRole::scopes`]); a credential only ever narrows that set: its scopes
//! are those it was created with, intersected with its creator's **current** role, so
//! demoting a member shrinks every key they issued.

use std::fmt;
use std::str::FromStr;

use strum::IntoEnumIterator as _;

/// One scope.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
)]
pub enum Scope {
    /// Connections, quota scopes and sending domains: read.
    #[strum(serialize = "connections:read")]
    ConnectionsRead,
    /// Connections, quota scopes and sending domains: create, change, verify, archive.
    #[strum(serialize = "connections:manage")]
    ConnectionsManage,
    /// People, fields, groups, segments, imports, exports and suppressions: read.
    #[strum(serialize = "people:read")]
    PeopleRead,
    /// People and the rest of the audience: write.
    #[strum(serialize = "people:write")]
    PeopleWrite,
    /// Campaigns and enrollments: read.
    #[strum(serialize = "campaigns:read")]
    CampaignsRead,
    /// Campaigns, enrollments and images: write.
    #[strum(serialize = "campaigns:write")]
    CampaignsWrite,
    /// Messages and delivery events: read.
    #[strum(serialize = "messages:read")]
    MessagesRead,
    /// Messages: send, cancel, resolve, release holds.
    #[strum(serialize = "messages:send")]
    MessagesSend,
    /// Threads and inbound messages: read.
    #[strum(serialize = "inbox:read")]
    InboxRead,
    /// Threads and inbound messages: update and review.
    #[strum(serialize = "inbox:write")]
    InboxWrite,
    /// Jobs, events, webhook endpoints and deliveries: read.
    #[strum(serialize = "automation:read")]
    AutomationRead,
    /// Jobs, events, webhook endpoints and deliveries: write.
    #[strum(serialize = "automation:manage")]
    AutomationManage,
    /// Reports.
    #[strum(serialize = "analytics:read")]
    AnalyticsRead,
    /// The workspace itself: read.
    #[strum(serialize = "workspace:read")]
    WorkspaceRead,
    /// The workspace's settings, members and keys (dashboard only).
    #[strum(serialize = "workspace:manage")]
    WorkspaceManage,
}

impl Scope {
    fn bit(self) -> u32 {
        // The enum is fieldless, so its discriminant is its position.
        let position = Self::iter()
            .position(|scope| scope == self)
            .unwrap_or_default();
        1_u32
            .checked_shl(u32::try_from(position).unwrap_or(31))
            .unwrap_or(0)
    }

    /// The scope as written on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A set of scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeSet(u32);

impl ScopeSet {
    /// Every scope.
    #[must_use]
    pub fn all() -> Self {
        Scope::iter().collect()
    }

    /// True when the set holds `scope`.
    #[must_use]
    pub fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// The scopes in both sets.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The scopes, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Scope> {
        Scope::iter().filter(move |scope| self.contains(*scope))
    }

    /// The scopes as strings, for storage and the wire.
    #[must_use]
    pub fn to_strings(self) -> Vec<String> {
        self.iter().map(|scope| scope.as_str().to_owned()).collect()
    }

    /// Parses stored or requested scopes; unknown names are refused.
    ///
    /// # Errors
    ///
    /// The first unknown scope name.
    pub fn parse<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        names
            .into_iter()
            .map(|name| Scope::from_str(name).map_err(|_| name.to_owned()))
            .collect()
    }
}

impl FromIterator<Scope> for ScopeSet {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        Self(iter.into_iter().fold(0, |bits, scope| bits | scope.bit()))
    }
}

/// A member's role in a workspace.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MembershipRole {
    Owner,
    Admin,
    Member,
    Viewer,
}

impl MembershipRole {
    /// The scopes the role grants: owners and admins hold every scope (owners alone may
    /// delete the workspace or transfer ownership, checked where those actions live);
    /// members read everything and write the audience, campaigns, messages, the inbox and
    /// their own connections; viewers only read.
    #[must_use]
    pub fn scopes(self) -> ScopeSet {
        use Scope::{
            AnalyticsRead, AutomationRead, CampaignsRead, CampaignsWrite, ConnectionsManage,
            ConnectionsRead, InboxRead, InboxWrite, MessagesRead, MessagesSend, PeopleRead,
            PeopleWrite, WorkspaceRead,
        };
        let reads = [
            ConnectionsRead,
            PeopleRead,
            CampaignsRead,
            MessagesRead,
            InboxRead,
            AutomationRead,
            AnalyticsRead,
            WorkspaceRead,
        ];
        match self {
            Self::Owner | Self::Admin => ScopeSet::all(),
            Self::Member => reads
                .into_iter()
                .chain([
                    PeopleWrite,
                    CampaignsWrite,
                    MessagesSend,
                    InboxWrite,
                    ConnectionsManage,
                ])
                .collect(),
            Self::Viewer => reads.into_iter().collect(),
        }
    }

    /// The role as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every role holds exactly its documented scopes, for every scope there is: owners and admins
    /// all of them, members every read plus the audience, campaigns, messages, the inbox and their
    /// connections, viewers only the reads. Nobody else holds `workspace:manage`, which is what
    /// keeps API keys, invitations and members in owners' and admins' hands. A new scope or role
    /// fails here until its expected holders are written down.
    #[test]
    fn each_role_holds_its_documented_scopes() {
        for role in MembershipRole::iter() {
            for scope in Scope::iter() {
                let read = scope.as_str().ends_with(":read");
                let expected = match role {
                    MembershipRole::Owner | MembershipRole::Admin => true,
                    MembershipRole::Member => {
                        read || matches!(
                            scope,
                            Scope::PeopleWrite
                                | Scope::CampaignsWrite
                                | Scope::MessagesSend
                                | Scope::InboxWrite
                                | Scope::ConnectionsManage
                        )
                    }
                    MembershipRole::Viewer => read,
                };
                assert_eq!(role.scopes().contains(scope), expected, "{role:?} {scope}");
            }
        }
    }

    /// A set parses what it prints, refuses an unknown name (naming it), and an intersection only
    /// ever narrows: a credential limited by its creator's role never gains a scope.
    #[test]
    fn scope_sets_round_trip_and_only_narrow() {
        let all = ScopeSet::all();
        assert_eq!(
            ScopeSet::parse(all.to_strings().iter().map(String::as_str)),
            Ok(all)
        );
        assert_eq!(
            ScopeSet::parse(["people:read", "root:everything"]),
            Err("root:everything".to_owned())
        );
        for role in MembershipRole::iter() {
            for granted in [ScopeSet::default(), MembershipRole::Viewer.scopes(), all] {
                let narrowed = granted.intersect(role.scopes());
                assert!(
                    narrowed
                        .iter()
                        .all(|scope| granted.contains(scope) && role.scopes().contains(scope))
                );
            }
        }
    }
}
