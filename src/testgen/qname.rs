//! Generates UUID-shaped SAM query names for fixture records.

use uuid::Uuid;

pub fn gen_qname() -> String {
    Uuid::new_v4().to_string()
}
