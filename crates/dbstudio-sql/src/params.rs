//! Parameter discovery and binding (client side substitution of `:name`, `$1`, `?`, `@p`).

use dbstudio_core::{Error, ParamRef, ParamValue, Result, ScriptDialect};

/// One replaced region: `[bound_start, bound_end)` of the bound text came from
/// `[orig_start, orig_end)` of the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    /// Start in the bound text.
    pub bound_start: u32,
    /// End (exclusive) in the bound text.
    pub bound_end: u32,
    /// Start in the original text.
    pub orig_start: u32,
    /// End (exclusive) in the original text.
    pub orig_end: u32,
}

/// Maps character offsets of the bound text back to the original text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OffsetMap {
    /// Replaced regions in ascending order of `bound_start`. Empty = identity.
    pub edits: Vec<Edit>,
}

impl OffsetMap {
    /// Identity map.
    pub fn identity() -> Self {
        Self::default()
    }

    /// Offset in the original text for `pos` in the bound text. A position inside a replaced
    /// region maps to the start of the original placeholder.
    pub fn to_original(&self, pos: u32) -> u32 {
        let mut delta: i64 = 0;
        for e in &self.edits {
            if pos < e.bound_start {
                break;
            }
            if pos < e.bound_end {
                return e.orig_start;
            }
            delta = i64::from(e.orig_end) - i64::from(e.bound_end);
        }
        u32::try_from((i64::from(pos) + delta).max(0)).unwrap_or(u32::MAX)
    }
}

/// Text with the parameters substituted plus the map back to the original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundScript {
    /// Text to send to the server.
    pub text: String,
    /// Position mapping.
    pub map: OffsetMap,
}

/// Parameters of `text`. Scaffold: none.
pub fn find_params(d: ScriptDialect, text: &str) -> Vec<ParamRef> {
    let _ = (d, text);
    Vec::new()
}

/// Substitute `params` into `text`. Scaffold: with no parameters the text is returned unchanged
/// with an identity map; otherwise `Unsupported`.
pub fn bind(d: ScriptDialect, text: &str, params: &[ParamValue]) -> Result<BoundScript> {
    let _ = d;
    if params.is_empty() {
        return Ok(BoundScript {
            text: text.to_string(),
            map: OffsetMap::identity(),
        });
    }
    Err(Error::Unsupported("query_params".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ParamInput;

    #[test]
    fn stub_contract_find_params_is_empty() {
        assert!(find_params(ScriptDialect::Postgres, "select :a, $1").is_empty());
    }

    #[test]
    fn stub_contract_bind_without_params_is_identity() {
        let b = bind(ScriptDialect::Postgres, "select 1", &[]).unwrap();
        assert_eq!(b.text, "select 1");
        assert_eq!(b.map, OffsetMap::identity());
        assert_eq!(b.map.to_original(7), 7);
    }

    #[test]
    fn stub_contract_bind_with_params_is_unsupported() {
        let p = ParamValue {
            name: ":a".into(),
            value: ParamInput::Null,
        };
        let err = bind(ScriptDialect::Postgres, "select :a", &[p]).unwrap_err();
        assert_eq!(err.kind(), "unsupported");
    }

    #[test]
    fn offset_map_maps_back_across_replacements() {
        // original: "a :id b" (":id" = 2..5) bound: "a 'xyz12' b" ('xyz12' = 2..9)
        let map = OffsetMap {
            edits: vec![Edit {
                bound_start: 2,
                bound_end: 9,
                orig_start: 2,
                orig_end: 5,
            }],
        };
        assert_eq!(map.to_original(1), 1);
        assert_eq!(map.to_original(4), 2);
        assert_eq!(map.to_original(9), 5);
        assert_eq!(map.to_original(10), 6);
    }
}
