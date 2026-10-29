use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

/// The 12 columns of `customers-2000000.csv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    Index,
    CustomerId,
    FirstName,
    LastName,
    Company,
    City,
    Country,
    Phone1,
    Phone2,
    Email,
    SubscriptionDate,
    Website,
}

impl Column {
    pub const ALL: [Column; 12] = [
        Column::Index,
        Column::CustomerId,
        Column::FirstName,
        Column::LastName,
        Column::Company,
        Column::City,
        Column::Country,
        Column::Phone1,
        Column::Phone2,
        Column::Email,
        Column::SubscriptionDate,
        Column::Website,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        let normalized = name
            .trim()
            .trim_matches('"')
            .trim_matches('`')
            .to_ascii_lowercase()
            .replace(' ', "_");
        match normalized.as_str() {
            "index" | "id" => Some(Column::Index),
            "customer_id" | "customerid" => Some(Column::CustomerId),
            "first_name" | "firstname" => Some(Column::FirstName),
            "last_name" | "lastname" => Some(Column::LastName),
            "company" => Some(Column::Company),
            "city" => Some(Column::City),
            "country" => Some(Column::Country),
            "phone_1" | "phone1" => Some(Column::Phone1),
            "phone_2" | "phone2" => Some(Column::Phone2),
            "email" => Some(Column::Email),
            "subscription_date" | "subscriptiondate" | "date" => Some(Column::SubscriptionDate),
            "website" => Some(Column::Website),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Column::Index => "index",
            Column::CustomerId => "customer_id",
            Column::FirstName => "first_name",
            Column::LastName => "last_name",
            Column::Company => "company",
            Column::City => "city",
            Column::Country => "country",
            Column::Phone1 => "phone_1",
            Column::Phone2 => "phone_2",
            Column::Email => "email",
            Column::SubscriptionDate => "subscription_date",
            Column::Website => "website",
        }
    }

    pub fn data_type(&self) -> &'static str {
        match self {
            Column::Index => "INTEGER (U32)",
            Column::SubscriptionDate => "DATE (YYYY-MM-DD)",
            _ => "TEXT",
        }
    }

    pub fn morton_indexed_info(&self) -> &'static str {
        match self {
            Column::Index => "Yes (morton3d[0], morton2d_id[0], PK)",
            Column::SubscriptionDate => "Yes (morton3d[1], morton2d_id[1])",
            Column::Country => "Yes (morton3d[2], morton2d_cc[0])",
            Column::City => "Yes (morton2d_cc[1])",
            _ => "No (post-filter)",
        }
    }
}

/// Dynamic SQL value for comparison and projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Int(i64),
    Str(String),
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::Str(s) => s.parse::<i64>().ok(),
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.as_i64().map(|v| v.max(0) as u32)
    }

    pub fn as_str(&self) -> String {
        match self {
            Value::Int(v) => v.to_string(),
            Value::Str(s) => s.clone(),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Str(a), Value::Str(b)) => a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()),
            (Value::Int(a), Value::Str(b)) => {
                if let Ok(b_int) = b.parse::<i64>() {
                    a.cmp(&b_int)
                } else {
                    a.to_string().cmp(b)
                }
            }
            (Value::Str(a), Value::Int(b)) => {
                if let Ok(a_int) = a.parse::<i64>() {
                    a_int.cmp(b)
                } else {
                    a.cmp(&b.to_string())
                }
            }
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(v) => write!(f, "{}", v),
            Value::Str(s) => write!(f, "{}", s),
        }
    }
}

/// A parsed row from `customers-2000000.csv`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerRow {
    pub index: u32,
    pub customer_id: String,
    pub first_name: String,
    pub last_name: String,
    pub company: String,
    pub city: String,
    pub country: String,
    pub phone_1: String,
    pub phone_2: String,
    pub email: String,
    pub subscription_date: String,
    pub website: String,
}

impl CustomerRow {
    pub fn from_string_record(rec: &csv::StringRecord) -> Option<Self> {
        if rec.len() < 12 {
            return None;
        }
        Some(Self {
            index: rec.get(0)?.trim().parse::<u32>().ok()?,
            customer_id: rec.get(1)?.to_string(),
            first_name: rec.get(2)?.to_string(),
            last_name: rec.get(3)?.to_string(),
            company: rec.get(4)?.to_string(),
            city: rec.get(5)?.to_string(),
            country: rec.get(6)?.to_string(),
            phone_1: rec.get(7)?.to_string(),
            phone_2: rec.get(8)?.to_string(),
            email: rec.get(9)?.to_string(),
            subscription_date: rec.get(10)?.to_string(),
            website: rec.get(11)?.to_string(),
        })
    }

    /// Serializes a row into a compact tab-separated payload for Redis Hash storage.
    pub fn to_compact_storage(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.index,
            self.customer_id,
            self.first_name,
            self.last_name,
            self.company,
            self.city,
            self.country,
            self.phone_1,
            self.phone_2,
            self.email,
            self.subscription_date,
            self.website
        )
    }

    /// Deserializes a row from our compact tab-separated Redis Hash payload.
    pub fn from_compact_storage(payload: &str) -> Option<Self> {
        let mut parts = payload.splitn(12, '\t');
        Some(Self {
            index: parts.next()?.parse::<u32>().ok()?,
            customer_id: parts.next()?.to_string(),
            first_name: parts.next()?.to_string(),
            last_name: parts.next()?.to_string(),
            company: parts.next()?.to_string(),
            city: parts.next()?.to_string(),
            country: parts.next()?.to_string(),
            phone_1: parts.next()?.to_string(),
            phone_2: parts.next()?.to_string(),
            email: parts.next()?.to_string(),
            subscription_date: parts.next()?.to_string(),
            website: parts.next()?.to_string(),
        })
    }

    pub fn get(&self, col: Column) -> Value {
        match col {
            Column::Index => Value::Int(self.index as i64),
            Column::CustomerId => Value::Str(self.customer_id.clone()),
            Column::FirstName => Value::Str(self.first_name.clone()),
            Column::LastName => Value::Str(self.last_name.clone()),
            Column::Company => Value::Str(self.company.clone()),
            Column::City => Value::Str(self.city.clone()),
            Column::Country => Value::Str(self.country.clone()),
            Column::Phone1 => Value::Str(self.phone_1.clone()),
            Column::Phone2 => Value::Str(self.phone_2.clone()),
            Column::Email => Value::Str(self.email.clone()),
            Column::SubscriptionDate => Value::Str(self.subscription_date.clone()),
            Column::Website => Value::Str(self.website.clone()),
        }
    }
}

