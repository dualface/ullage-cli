use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeProfile {
    #[serde(default)]
    pub account: Option<ClaudeAccount>,
    #[serde(default)]
    pub organization: Option<ClaudeOrganization>,
    #[serde(default, alias = "subscriptionType")]
    pub subscription_type: Option<String>,
    #[serde(default, alias = "billingType")]
    pub billing_type: Option<String>,
    #[serde(default, alias = "claudeCodeTrialEndsAt")]
    pub claude_code_trial_ends_at: Option<DateTime<Utc>>,
}

impl ClaudeProfile {
    /// Stable identity of the signed-in account: the account UUID when
    /// Anthropic sends one, the email otherwise. Never a display name, which
    /// two accounts can share and which therefore must not decide that one of
    /// them supersedes the other.
    pub(crate) fn account_key(&self) -> Option<String> {
        self.account
            .as_ref()
            .and_then(|account| {
                account
                    .uuid
                    .clone()
                    .or_else(|| account.email_address.clone())
            })
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    pub(crate) fn account_label(&self) -> Option<String> {
        self.account.as_ref().and_then(|account| {
            account
                .email_address
                .clone()
                .or_else(|| account.full_name.clone())
                .or_else(|| account.display_name.clone())
        })
    }

    pub(crate) fn plan(&self) -> Option<String> {
        let organization = self.organization.as_ref();
        let raw = self
            .subscription_type
            .as_deref()
            .or_else(|| organization.and_then(|value| value.subscription_type.as_deref()))
            .or_else(|| organization.and_then(|value| value.organization_type.as_deref()))
            .or(self.billing_type.as_deref())
            .or_else(|| organization.and_then(|value| value.billing_type.as_deref()))?;
        let tier = organization.and_then(|value| value.rate_limit_tier.as_deref());
        Some(normalize_plan(raw, tier))
    }

    pub(crate) fn trial_ends_at(&self) -> Option<DateTime<Utc>> {
        self.claude_code_trial_ends_at.or_else(|| {
            self.organization
                .as_ref()
                .and_then(|value| value.claude_code_trial_ends_at)
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeAccount {
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default, alias = "email")]
    pub email_address: Option<String>,
    #[serde(default, alias = "fullName")]
    pub full_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeOrganization {
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub organization_type: Option<String>,
    #[serde(default, alias = "billingType")]
    pub billing_type: Option<String>,
    #[serde(default, alias = "subscriptionType")]
    pub subscription_type: Option<String>,
    #[serde(default, alias = "rateLimitTier")]
    pub rate_limit_tier: Option<String>,
    #[serde(default, alias = "claudeCodeTrialEndsAt")]
    pub claude_code_trial_ends_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaudeUsageResponse {
    #[serde(default)]
    pub five_hour: Option<ClaudeUsageWindow>,
    #[serde(default)]
    pub seven_day: Option<ClaudeUsageWindow>,
    #[serde(default)]
    pub seven_day_opus: Option<ClaudeUsageWindow>,
    #[serde(default)]
    pub seven_day_sonnet: Option<ClaudeUsageWindow>,
    #[serde(default)]
    pub seven_day_oauth_apps: Option<ClaudeUsageWindow>,
    #[serde(default)]
    pub extra_usage: Option<ClaudeExtraUsage>,
    #[serde(default)]
    pub limits: Vec<ClaudeLimit>,
    #[serde(default)]
    pub subscription_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaudeUsageWindow {
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaudeExtraUsage {
    #[serde(default)]
    pub is_enabled: bool,
    #[serde(default)]
    pub monthly_limit: Option<f64>,
    #[serde(default)]
    pub used_credits: Option<f64>,
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaudeLimit {
    pub kind: String,
    pub group: String,
    pub percent: f64,
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scope: Option<ClaudeLimitScope>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeLimitScope {
    #[serde(default)]
    pub model: Option<ClaudeScopeLabel>,
    #[serde(default)]
    pub surface: Option<ClaudeScopeLabel>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeScopeLabel {
    pub display_name: String,
}

pub(crate) fn normalize_plan(raw: &str, tier: Option<&str>) -> String {
    let value = raw.trim().to_ascii_lowercase().replace('-', "_");
    let tier = tier
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_");

    if value.contains("enterprise") {
        "enterprise".into()
    } else if value.contains("team") {
        "team".into()
    } else if value.contains("max") || tier.contains("max") {
        if value.contains("20x") || tier.contains("20x") {
            "max_20x".into()
        } else if value.contains("5x") || tier.contains("5x") {
            "max_5x".into()
        } else {
            "max".into()
        }
    } else if value.contains("pro") {
        "pro".into()
    } else if value.contains("free") {
        "free".into()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_key_never_falls_back_to_a_shareable_display_name() {
        let with_uuid: ClaudeProfile =
            serde_json::from_str(r#"{"account":{"uuid":"acct-1","email":"user@example.invalid"}}"#)
                .unwrap();
        assert_eq!(with_uuid.account_key().as_deref(), Some("acct-1"));

        let email_only: ClaudeProfile =
            serde_json::from_str(r#"{"account":{"email":" user@example.invalid "}}"#).unwrap();
        assert_eq!(
            email_only.account_key().as_deref(),
            Some("user@example.invalid")
        );

        // Two people can share a name, and this value decides whether one
        // account supersedes another, so a name must not produce a key.
        let name_only: ClaudeProfile =
            serde_json::from_str(r#"{"account":{"full_name":"Fixture User"}}"#).unwrap();
        assert_eq!(name_only.account_key(), None);
        assert_eq!(name_only.account_label().as_deref(), Some("Fixture User"));

        let empty: ClaudeProfile = serde_json::from_str(r#"{"account":{"uuid":"  "}}"#).unwrap();
        assert_eq!(empty.account_key(), None);
    }

    #[test]
    fn maps_known_plans_without_rejecting_unknown_values() {
        assert_eq!(normalize_plan("claude_pro", None), "pro");
        assert_eq!(
            normalize_plan("claude_max", Some("default_claude_max_20x")),
            "max_20x"
        );
        assert_eq!(normalize_plan("future-plan", None), "future_plan");
    }

    #[test]
    fn deserializes_account_when_full_name_and_display_name_both_present() {
        let profile: ClaudeProfile = serde_json::from_str(
            r#"{
                "account": {
                    "uuid": "00000000-0000-4000-8000-000000000001",
                    "full_name": "Fixture User",
                    "display_name": "Fixture User",
                    "email": "user@example.invalid"
                }
            }"#,
        )
        .unwrap();
        let account = profile.account.as_ref().unwrap();
        assert_eq!(account.full_name.as_deref(), Some("Fixture User"));
        assert_eq!(account.display_name.as_deref(), Some("Fixture User"));
        assert_eq!(
            account.email_address.as_deref(),
            Some("user@example.invalid")
        );
    }

    #[test]
    fn account_label_falls_back_across_email_full_name_and_display_name() {
        let cases = [
            (
                r#"{"email":"user@example.invalid"}"#,
                Some("user@example.invalid"),
            ),
            (r#"{"full_name":"Fixture User"}"#, Some("Fixture User")),
            (r#"{"display_name":"Shown Name"}"#, Some("Shown Name")),
            (
                r#"{"email":"user@example.invalid","full_name":"Fixture User"}"#,
                Some("user@example.invalid"),
            ),
            (
                r#"{"email":"user@example.invalid","display_name":"Shown Name"}"#,
                Some("user@example.invalid"),
            ),
            (
                r#"{"full_name":"Fixture User","display_name":"Shown Name"}"#,
                Some("Fixture User"),
            ),
            (
                r#"{"email":"user@example.invalid","full_name":"Fixture User","display_name":"Shown Name"}"#,
                Some("user@example.invalid"),
            ),
            (r#"{}"#, None),
        ];

        for (account, expected) in cases {
            let profile: ClaudeProfile =
                serde_json::from_str(&format!(r#"{{"account":{account}}}"#)).unwrap();
            assert_eq!(
                profile.account_label().as_deref(),
                expected,
                "account JSON {account}"
            );
        }
    }

    #[test]
    fn parses_live_profile_shape_with_full_name_and_display_name() {
        let profile: ClaudeProfile =
            serde_json::from_str(include_str!("../tests/fixtures/profile-live-shape.json"))
                .unwrap();
        assert_eq!(
            profile.account_label().as_deref(),
            Some("user@example.invalid")
        );
        assert_eq!(profile.plan().as_deref(), Some("max_5x"));
    }
}
