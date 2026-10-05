//! Storage encodings of the domain's types: how they are written to and read from PostgreSQL.
//!
//! `sqlx` has no `jiff` support, so [`Timestamp`] and [`Date`] carry their own PostgreSQL
//! encodings: `timestamptz` is microseconds since 2000-01-01 UTC, `date` is days since
//! 2000-01-01, both in the binary protocol. Queries name them with an override
//! (`created_at AS "created_at: Timestamp"`).

use jiff::civil;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef};
use sqlx::{Decode, Encode, Postgres, Type};

use crate::domain::ids::{Id, WorkspaceId};
use crate::domain::time::{Date, Timestamp};

const UNIX_MICROS_AT_PG_EPOCH: i64 = 946_684_800_000_000;
const PG_EPOCH_DATE: civil::Date = civil::date(2000, 1, 1);

impl Type<Postgres> for Timestamp {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_name("timestamptz")
    }
}

impl PgHasArrayType for Timestamp {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::with_name("_timestamptz")
    }
}

impl Encode<'_, Postgres> for Timestamp {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        let micros = self
            .0
            .as_microsecond()
            .checked_sub(UNIX_MICROS_AT_PG_EPOCH)
            .ok_or("timestamp out of range")?;
        <i64 as Encode<Postgres>>::encode_by_ref(&micros, buf)
    }
}

impl<'r> Decode<'r, Postgres> for Timestamp {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            PgValueFormat::Binary => {
                let micros = <i64 as Decode<Postgres>>::decode(value)?
                    .checked_add(UNIX_MICROS_AT_PG_EPOCH)
                    .ok_or("timestamp out of range")?;
                Ok(Self(jiff::Timestamp::from_microsecond(micros)?))
            }
            PgValueFormat::Text => {
                let text = value.as_str()?;
                let parsed = text
                    .parse::<jiff::Timestamp>()
                    .or_else(|_| text.replacen(' ', "T", 1).parse::<jiff::Timestamp>())?;
                Ok(Self(parsed))
            }
        }
    }
}

impl Type<Postgres> for Date {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_name("date")
    }
}

impl Encode<'_, Postgres> for Date {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        let days = self.0.since(PG_EPOCH_DATE)?.get_days();
        <i32 as Encode<Postgres>>::encode_by_ref(&days, buf)
    }
}

impl<'r> Decode<'r, Postgres> for Date {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            PgValueFormat::Binary => {
                let days = <i32 as Decode<Postgres>>::decode(value)?;
                Ok(Self(
                    PG_EPOCH_DATE.checked_add(jiff::Span::new().try_days(days)?)?,
                ))
            }
            PgValueFormat::Text => Ok(Self(value.as_str()?.parse::<civil::Date>()?)),
        }
    }
}

impl<R> Type<Postgres> for Id<R> {
    fn type_info() -> PgTypeInfo {
        <uuid::Uuid as Type<Postgres>>::type_info()
    }
}

impl<R> PgHasArrayType for Id<R> {
    fn array_type_info() -> PgTypeInfo {
        <uuid::Uuid as PgHasArrayType>::array_type_info()
    }
}

impl<R> Encode<'_, Postgres> for Id<R> {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <uuid::Uuid as Encode<Postgres>>::encode_by_ref(&self.uuid(), buf)
    }
}

impl<'r, R> Decode<'r, Postgres> for Id<R> {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(Self::from_uuid(<uuid::Uuid as Decode<Postgres>>::decode(
            value,
        )?))
    }
}

impl Type<Postgres> for WorkspaceId {
    fn type_info() -> PgTypeInfo {
        <uuid::Uuid as Type<Postgres>>::type_info()
    }
}

impl Encode<'_, Postgres> for WorkspaceId {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <uuid::Uuid as Encode<Postgres>>::encode_by_ref(&self.uuid(), buf)
    }
}
