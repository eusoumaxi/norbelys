//! Pagination: cursors only, signed and bound to the query that produced them.
//!
//! Offsets are not offered: on a table that grows while a client reads it, an offset skips
//! or repeats rows and costs a scan of everything before it. A cursor instead names the last
//! row seen (its sort key and its id), and the next page starts strictly after it, which is
//! an index range scan whatever the depth.
//!
//! How a list uses it: parse [`ListQuery`] into [`PageParams`] with
//! [`PageParams::from_query`] (a list with one order) or [`PageParams::sorted`] (a list that
//! offers sorts), fetch `limit + 1` rows after [`PageParams::after`] in the chosen order, and
//! build the response with [`Page::new`]; the extra row only tells whether another page
//! exists.
//!
//! # Orders
//!
//! The default order is `id` descending. Ids are UUIDv7, minted when their row is created, so
//! this is newest first, and it is the creation order too: a separate `created_at` sort would
//! repeat it. `?order=asc` reverses any order. A list may offer other sorts with `?sort=`
//! ([`Sort`]), each on a timestamp column its rows never leave `NULL` and each served by an
//! index that ends with the id. Every sort ends with `id`, so the order is total and a page
//! boundary between rows sharing a key is exact. A sort on a column that changes
//! (`updated_at`, `last_activity_at`) can move a row between pages while a client reads them;
//! a complete, stable traversal is an export.
//!
//! # Cursors
//!
//! The cursor is the base64url of a small JSON payload, a dot, and the base64url of an HMAC
//! over that payload. The payload binds the cursor to the workspace, the resource, the sort,
//! the order and a hash of the filters, and carries the last row's sort key typed by its sort
//! ([`Key`]). A cursor that was tampered with, that was minted for any other query (another
//! sort of the same list included), or whose key does not fit its sort is refused with
//! `400 invalid_request` and `errors[].code = "invalid_cursor"`. Opaque here means
//! tamper-evident, not encrypted.

use std::str::FromStr as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::{self, Keys};
use crate::domain::ids::WorkspaceId;
use crate::domain::time::Timestamp;
use crate::problem::{Code, FieldError, Problem};

/// The default and the largest page.
const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;
/// Exact counts stop here; above it the count is reported as capped.
pub const COUNT_CAP: i64 = 10_000;
/// The name of the default order, by id.
const ID: &str = "id";

/// The query parameters every list accepts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
    pub order: Option<Order>,
    /// `?sort=`: one of the sorts the list offers, `id` when absent. Read as text so that an
    /// unknown name is refused naming the sorts the list offers.
    pub sort: Option<String>,
    /// `?include=`: what the page adds to its `meta`. Like every request enum it is closed: an
    /// unknown value is refused rather than ignored, so a typo never silently drops the count.
    pub include: Option<Include>,
}

/// The direction of a list's order (`?order=`): `desc`, the default, is newest first under the
/// `id` sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = ListOrder)]
pub enum Order {
    #[default]
    Desc,
    Asc,
}

/// What a list adds to its page's `meta` on request (`?include=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = ListInclude)]
pub enum Include {
    /// `meta.total_count`: the rows the filters match, exact up to 10,000, with
    /// `meta.total_count_capped` above.
    TotalCount,
}

/// A sort a list can offer with `?sort=`, named after the field it sorts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Sort {
    /// The id: the creation order, and every list's default.
    Id,
    /// When the row last changed.
    UpdatedAt,
    /// When a thread last had a message, sent or received.
    LastActivityAt,
}

impl Sort {
    /// The sort's name on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The sort key of the last row a page showed, typed by the sort that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Key {
    /// An instant: the key of the timestamp sorts.
    At(Timestamp),
    /// Another row's id: the key of a list ordered by a parent row first (webhook deliveries,
    /// by their event).
    Row(Uuid),
}

/// The position a page starts after: the sort key's value and the row's id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct After {
    /// The sort key of the last row seen; `None` for the `id` sort.
    pub key: Option<Key>,
    /// The id of the last row seen.
    pub id: Uuid,
}

#[derive(Serialize, Deserialize)]
struct Payload {
    v: u8,
    workspace: Uuid,
    resource: String,
    sort: String,
    order: Order,
    filters_hash: String,
    key: Option<Key>,
    id: Uuid,
}

/// What a list operation needs to query one page.
#[derive(Debug, Clone)]
pub struct PageParams {
    /// Rows to return; the query fetches `limit + 1`.
    pub limit: i64,
    pub order: Order,
    /// Where the page starts; `None` for the first page.
    pub after: Option<After>,
    /// `?include=total_count`.
    pub include_total: bool,
    sort: Sort,
    binding: Binding,
}

#[derive(Debug, Clone)]
struct Binding {
    workspace: Uuid,
    resource: &'static str,
    /// The order's name, as the cursor records it.
    sort: &'static str,
    /// The order is one of the timestamp sorts, whose key is an instant.
    instant: bool,
    filters_hash: String,
}

impl PageParams {
    /// Validates the query and the cursor of a list with one order, named `sort` (`id`, or the
    /// order of a list sorted by a parent row first), for this workspace, resource and filters.
    ///
    /// # Errors
    ///
    /// `422` for a limit out of range or a `?sort=` other than the list's order;
    /// `400 invalid_cursor` for a cursor that is not ours or not this query's.
    pub fn from_query(
        keys: &Keys,
        workspace: WorkspaceId,
        resource: &'static str,
        sort: &'static str,
        filters: &impl Serialize,
        query: &ListQuery,
    ) -> Result<Self, Problem> {
        if query.sort.as_deref().is_some_and(|asked| asked != sort) {
            return Err(unsortable(&[sort]));
        }
        Self::bound(
            keys,
            Binding {
                workspace: workspace.uuid(),
                resource,
                sort,
                instant: false,
                filters_hash: hash(filters),
            },
            Sort::Id,
            query,
        )
    }

    /// Validates the query and the cursor of a list that offers the sorts `offered` besides its
    /// default `id`: `?sort=` chooses among them.
    ///
    /// # Errors
    ///
    /// `422` for a limit out of range or a sort the list does not offer; `400 invalid_cursor`
    /// for a cursor that is not ours or not this query's, a cursor of another sort included.
    pub fn sorted(
        keys: &Keys,
        workspace: WorkspaceId,
        resource: &'static str,
        offered: &[Sort],
        filters: &impl Serialize,
        query: &ListQuery,
    ) -> Result<Self, Problem> {
        let sort = match query.sort.as_deref() {
            None => Sort::Id,
            Some(name) => Sort::from_str(name)
                .ok()
                .filter(|sort| *sort == Sort::Id || offered.contains(sort))
                .ok_or_else(|| {
                    let names: Vec<&str> = std::iter::once(ID)
                        .chain(offered.iter().map(|sort| sort.as_str()))
                        .collect();
                    unsortable(&names)
                })?,
        };
        Self::bound(
            keys,
            Binding {
                workspace: workspace.uuid(),
                resource,
                sort: sort.as_str(),
                instant: sort != Sort::Id,
                filters_hash: hash(filters),
            },
            sort,
            query,
        )
    }

    /// Checks the limit and the cursor against `binding`.
    fn bound(
        keys: &Keys,
        binding: Binding,
        sort: Sort,
        query: &ListQuery,
    ) -> Result<Self, Problem> {
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(Problem::invalid_field(
                "?limit",
                "range",
                "`limit` is between 1 and 100.",
            ));
        }
        let order = query.order.unwrap_or_default();
        let after = match &query.cursor {
            None => None,
            Some(cursor) => Some(decode(keys, cursor, &binding, order)?),
        };
        Ok(Self {
            limit,
            order,
            after,
            include_total: query.include == Some(Include::TotalCount),
            sort,
            binding,
        })
    }

    /// The row limit to fetch: one more than the page, to know whether more exist.
    #[must_use]
    pub fn fetch(&self) -> i64 {
        self.limit + 1
    }

    /// The id the page starts after: the whole position for the `id` sort, the tie-breaker of
    /// any other.
    #[must_use]
    pub fn after_id(&self) -> Option<Uuid> {
        self.after.as_ref().map(|after| after.id)
    }

    /// The instant the page starts after, for a timestamp sort; `None` on the first page.
    #[must_use]
    pub fn after_at(&self) -> Option<Timestamp> {
        match self.after.as_ref().and_then(|after| after.key) {
            Some(Key::At(at)) => Some(at),
            Some(Key::Row(_)) | None => None,
        }
    }

    /// True for ascending order.
    #[must_use]
    pub fn ascending(&self) -> bool {
        self.order == Order::Asc
    }

    /// The sort the client chose: [`Sort::Id`] by default, and always for a list with one order.
    #[must_use]
    pub fn sort(&self) -> Sort {
        self.sort
    }

    /// The cursor position of a row of a list that offers sorts: its id, with `at`, its value of
    /// the sorted field, when the sort is not by id.
    #[must_use]
    pub fn position(&self, at: Timestamp, id: Uuid) -> After {
        After {
            key: (self.sort != Sort::Id).then_some(Key::At(at)),
            id,
        }
    }
}

/// The hash of a list's filters, which a cursor is bound to.
fn hash(filters: &impl Serialize) -> String {
    crypto::hex(&crypto::sha256(
        &serde_json::to_vec(filters).unwrap_or_default(),
    ))
}

/// `422` on `?sort`, naming the sorts the list offers.
fn unsortable(names: &[&str]) -> Problem {
    let names: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    Problem::invalid_field(
        "?sort",
        "invalid",
        format!("`sort` is one of {}.", names.join(", ")),
    )
}

fn invalid_cursor() -> Problem {
    Problem {
        code: Code::InvalidRequest,
        detail: "The cursor is not valid for this request.".to_owned(),
        errors: vec![FieldError {
            pointer: "?cursor".to_owned(),
            code: "invalid_cursor".to_owned(),
            detail: "Use the `next_cursor` of the previous page of the same request.".to_owned(),
        }],
        retry_after: None,
    }
}

fn decode(keys: &Keys, cursor: &str, binding: &Binding, order: Order) -> Result<After, Problem> {
    let (payload, tag) = cursor.split_once('.').ok_or_else(invalid_cursor)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| invalid_cursor())?;
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| invalid_cursor())?;
    if !keys.verify_cursor(&payload, &tag) {
        return Err(invalid_cursor());
    }
    let payload: Payload = serde_json::from_slice(&payload).map_err(|_| invalid_cursor())?;
    // The key must be the kind its sort writes: none for `id`, an instant for a timestamp sort,
    // a row's id for a list ordered by a parent row first.
    let shaped = match payload.key {
        None => binding.sort == ID,
        Some(Key::At(_)) => binding.instant,
        Some(Key::Row(_)) => binding.sort != ID && !binding.instant,
    };
    let matches = payload.v == 1
        && payload.workspace == binding.workspace
        && payload.resource == binding.resource
        && payload.sort == binding.sort
        && payload.order == order
        && payload.filters_hash == binding.filters_hash
        && shaped;
    if !matches {
        return Err(invalid_cursor());
    }
    Ok(After {
        key: payload.key,
        id: payload.id,
    })
}

fn encode(keys: &Keys, binding: &Binding, order: Order, after: After) -> String {
    let payload = Payload {
        v: 1,
        workspace: binding.workspace,
        resource: binding.resource.to_owned(),
        sort: binding.sort.to_owned(),
        order,
        filters_hash: binding.filters_hash.clone(),
        key: after.key,
        id: after.id,
    };
    let json = serde_json::to_vec(&payload).unwrap_or_default();
    let tag = keys.sign_cursor(&json);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(json),
        URL_SAFE_NO_PAD.encode(tag)
    )
}

/// The page metadata.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Meta {
    pub has_more: bool,
    pub next_cursor: Option<String>,
    /// With `?include=total_count`: exact up to 10,000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
    /// With `?include=total_count`: true when the count stopped at 10,000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count_capped: Option<bool>,
}

/// One page of a list.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Page<T> {
    /// At most `limit` rows, 100 at most.
    #[schema(max_items = 100)]
    pub data: Vec<T>,
    pub meta: Meta,
}

impl<T> Page<T> {
    /// Builds the page from up to `limit + 1` rows in order; `key_of` gives a row's sort key
    /// and id for the next cursor.
    pub fn new(
        keys: &Keys,
        params: &PageParams,
        mut rows: Vec<T>,
        key_of: impl Fn(&T) -> After,
    ) -> Self {
        let limit = usize::try_from(params.limit).unwrap_or(usize::MAX);
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last()
                .map(|last| encode(keys, &params.binding, params.order, key_of(last)))
        } else {
            None
        };
        Self {
            data: rows,
            meta: Meta {
                has_more,
                next_cursor,
                total_count: None,
                total_count_capped: None,
            },
        }
    }

    /// Builds an id-ordered page, adding the capped count when the caller requested it.
    pub fn from_ids(
        keys: &Keys,
        params: &PageParams,
        rows: Vec<T>,
        total: Option<i64>,
        id: impl Fn(&T) -> Uuid,
    ) -> Self {
        let page = Self::new(keys, params, rows, |row| by_id(id(row)));
        match total {
            Some(total) => page.with_total(total),
            None => page,
        }
    }

    /// Adds the capped exact count.
    #[must_use]
    pub fn with_total(mut self, counted: i64) -> Self {
        self.meta.total_count = Some(counted.min(COUNT_CAP));
        self.meta.total_count_capped = Some(counted > COUNT_CAP);
        self
    }
}

/// The cursor position of a row sorted by `id` alone.
#[must_use]
pub fn by_id(id: Uuid) -> After {
    After { key: None, id }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{ListQuery, Page, PageParams, Sort};
    use crate::domain::ids::WorkspaceId;
    use crate::domain::time::Timestamp;
    use crate::problem::Code;
    use crate::testing;

    /// A query for one row per page, sorted by `sort`, after `cursor`.
    fn query(sort: Option<&str>, cursor: Option<String>) -> ListQuery {
        ListQuery {
            limit: Some(1),
            cursor,
            sort: sort.map(str::to_owned),
            ..ListQuery::default()
        }
    }

    /// The cursor of a page sorted by an instant carries that instant and the row's id, so the
    /// next page starts exactly after the row (the instant alone would skip or repeat the rows
    /// that share it). The same cursor under the list's other sort is refused: its key means
    /// nothing there, and following it would silently skip or repeat rows.
    #[test]
    fn a_sorted_cursor_carries_its_instant_and_binds_its_sort() {
        let keys = testing::keys();
        let workspace = WorkspaceId::trusted(Uuid::now_v7());
        let offered = [Sort::LastActivityAt];
        let sorted = |sort: Option<&str>, cursor: Option<String>| {
            PageParams::sorted(
                &keys,
                workspace,
                "threads",
                &offered,
                &(),
                &query(sort, cursor),
            )
        };
        let first = sorted(Some("last_activity_at"), None).unwrap();
        assert_eq!(first.sort(), Sort::LastActivityAt);
        let at = Timestamp(jiff::Timestamp::from_microsecond(1_790_000_000_123_456).unwrap());
        let rows = vec![Uuid::now_v7(), Uuid::now_v7()];
        let page = Page::new(&keys, &first, rows.clone(), |id| first.position(at, *id));
        assert!(page.meta.has_more);
        let cursor = page.meta.next_cursor.unwrap();

        let next = sorted(Some("last_activity_at"), Some(cursor.clone())).unwrap();
        assert_eq!(next.after_at(), Some(at));
        assert_eq!(next.after_id(), Some(rows[0]));

        for other in [None, Some("id")] {
            let refused = sorted(other, Some(cursor.clone())).unwrap_err();
            assert_eq!(refused.code, Code::InvalidRequest, "{other:?}");
            assert_eq!(refused.errors[0].code, "invalid_cursor");
        }
    }

    /// A list answers only the sorts it offers: another known sort, `created_at` (the `id`
    /// order already) and an unknown name are `422` on `?sort`, and so is any sort but its own
    /// on a list with one order. A silent fallback to the default order would hand a client
    /// rows in an order it did not ask for.
    #[test]
    fn a_sort_the_list_does_not_offer_is_refused() {
        let keys = testing::keys();
        let workspace = WorkspaceId::trusted(Uuid::now_v7());
        for name in ["updated_at", "created_at", "nope"] {
            let refused = PageParams::sorted(
                &keys,
                workspace,
                "threads",
                &[Sort::LastActivityAt],
                &(),
                &query(Some(name), None),
            )
            .unwrap_err();
            assert_eq!(refused.code, Code::ValidationFailed, "{name}");
            assert_eq!(refused.errors[0].pointer, "?sort");
        }
        let one_order = |sort: &str| {
            PageParams::from_query(
                &keys,
                workspace,
                "fields",
                "id",
                &(),
                &query(Some(sort), None),
            )
        };
        assert_eq!(
            one_order("updated_at").unwrap_err().errors[0].pointer,
            "?sort"
        );
        assert!(one_order("id").is_ok());
    }
}
