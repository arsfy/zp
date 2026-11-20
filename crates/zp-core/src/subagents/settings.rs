use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zp_ai::ThinkingLevel;

/// How a child gets its model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Assignment {
    #[default]
    FollowMainModel,
    TaskProfiles,
}

/// Whether the user confirms each delegation.
///
/// Tasks start on their own by default: subagents only read the project, which the main
/// agent's own read tools may do without asking, and their models are the main model or
/// ones the user chose for a profile. Asking remains an option, and a project can require it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Approval {
    Ask,
    #[default]
    Allowlisted,
}

/// A task role and the model approved for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// A qualified provider/model ID, or `inherit`.
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub thinking: ThinkingLevel,
}

/// Personal delegation settings. A repository can only restrict the effective policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment: Option<Assignment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
    /// Optional project restriction on the IDs of personal profiles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_profiles: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_run: Option<usize>,
    /// Model requests per task; unset for no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_model_requests_per_task: Option<usize>,
    /// Seconds a task may run; unset for no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Output tokens per model request of a child; unset uses the model's own limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

impl SubagentSettings {
    /// The effective policy: personal settings, restricted by a trusted project's.
    pub fn effective(personal: Option<&Self>, project: Option<&Self>) -> Self {
        personal.cloned().unwrap_or_default().restricted_by(project)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled == Some(true)
    }

    /// Whether delegation can run: enabled, and with a model to send tasks to.
    pub fn is_usable(&self) -> bool {
        self.is_enabled()
            && (self.assignment() == Assignment::FollowMainModel || !self.profiles.is_empty())
    }

    pub fn assignment(&self) -> Assignment {
        self.assignment.unwrap_or_default()
    }

    pub fn approval(&self) -> Approval {
        self.approval.unwrap_or_default()
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent.unwrap_or(4).clamp(1, 16)
    }

    pub fn max_per_run(&self) -> usize {
        self.max_per_run.unwrap_or(4).clamp(1, 64)
    }

    /// Model requests a task may make, if limited.
    pub fn max_model_requests_per_task(&self) -> Option<usize> {
        self.max_model_requests_per_task
            .map(|requests| requests.clamp(1, MAX_REQUESTS_PER_TASK))
    }

    /// Seconds a task may run once it has a slot (time spent queued does not count), if
    /// limited.
    pub fn timeout_seconds(&self) -> Option<u64> {
        self.timeout_seconds
            .map(|seconds| seconds.clamp(1, MAX_TIMEOUT_SECONDS))
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens.map(|tokens| tokens.max(1))
    }

    /// Applies restrictions from a trusted project's settings.
    pub fn restricted_by(mut self, project: Option<&Self>) -> Self {
        let Some(project) = project else { return self };
        if project.enabled == Some(false) {
            self.enabled = Some(false);
        }
        if project.approval == Some(Approval::Ask) {
            self.approval = Some(Approval::Ask);
        }
        if let Some(allowed) = &project.allowed_profiles {
            self.profiles.retain(|id, _| allowed.contains(id));
            if self
                .default_profile
                .as_ref()
                .is_some_and(|id| !allowed.contains(id))
            {
                self.default_profile = None;
            }
        }
        if let Some(limit) = project.max_concurrent {
            self.max_concurrent = Some(self.max_concurrent().min(limit.max(1)));
        }
        if let Some(limit) = project.max_per_run {
            self.max_per_run = Some(self.max_per_run().min(limit.max(1)));
        }
        // Optional limits: the project's applies when tighter or when there is none.
        if let Some(limit) = project.max_model_requests_per_task {
            self.max_model_requests_per_task =
                Some(tighter(self.max_model_requests_per_task(), limit));
        }
        if let Some(limit) = project.timeout_seconds {
            self.timeout_seconds = Some(tighter(self.timeout_seconds(), limit));
        }
        if let Some(limit) = project.max_output_tokens {
            self.max_output_tokens = Some(tighter(self.max_output_tokens(), limit));
        }
        self
    }
}

/// Largest model request and timeout limits accepted.
pub const MAX_REQUESTS_PER_TASK: usize = 1_000;
pub const MAX_TIMEOUT_SECONDS: u64 = 86_400;

/// The tighter of an optional limit and `limit` (at least 1).
fn tighter<T: Ord + From<u8>>(current: Option<T>, limit: T) -> T {
    let limit = limit.max(T::from(1));
    match current {
        Some(current) => current.min(limit),
        None => limit,
    }
}

pub fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::{Approval, Assignment, SubagentSettings};

    #[test]
    fn a_project_can_only_restrict_personal_delegation_settings() {
        let global: SubagentSettings = serde_json::from_str(
            r#"{
            "enabled": true, "assignment": "taskProfiles", "approval": "allowlisted",
            "defaultProfile": "review",
            "profiles": {"review": {"model": "one/review"}, "explore": {"model": "two/explore"}},
            "maxConcurrent": 2, "maxPerRun": 6
        }"#,
        )
        .unwrap();
        let project: SubagentSettings = serde_json::from_str(
            r#"{
            "enabled": true, "assignment": "followMainModel", "approval": "ask",
            "profiles": {"explore": {"model": "other/model"}},
            "allowedProfiles": ["explore"], "maxConcurrent": 4, "maxPerRun": 3,
            "maxOutputTokens": 16000, "timeoutSeconds": 300
        }"#,
        )
        .unwrap();
        assert_eq!(global.max_output_tokens(), None, "no limit by default");
        let effective = SubagentSettings::effective(Some(&global), Some(&project));
        assert_eq!(effective.max_output_tokens(), Some(16_000));
        assert_eq!(
            effective.timeout_seconds(),
            Some(300),
            "a project can add a limit"
        );
        assert!(effective.is_enabled());
        let defaults = SubagentSettings::default();
        assert_eq!(
            defaults.approval(),
            Approval::Allowlisted,
            "tasks start on their own"
        );
        assert_eq!(defaults.max_concurrent(), 4);
        assert_eq!(
            defaults.max_model_requests_per_task(),
            None,
            "no request limit by default"
        );
        assert_eq!(defaults.timeout_seconds(), None, "no time limit by default");
        assert_eq!(effective.assignment(), Assignment::TaskProfiles);
        assert_eq!(effective.approval(), Approval::Ask);
        assert_eq!(effective.max_concurrent(), 2);
        assert_eq!(effective.max_per_run(), 3);
        assert_eq!(effective.profiles["explore"].model, "two/explore");
        assert_eq!(effective.profiles.len(), 1);
        assert!(effective.default_profile.is_none());
        assert!(
            !SubagentSettings::default()
                .restricted_by(Some(&project))
                .is_enabled()
        );
    }
}
