//! Account mutation helpers for the daemon engine.

use ullage_core::summary::MetricFilter;

use crate::model::{AccountConfig, AccountId, DaemonError};

use super::DaemonEngine;

impl DaemonEngine {
    pub async fn set_account_enabled(
        &self,
        account_id: &AccountId,
        enabled: bool,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let accounts = self.inner.accounts.write().await;
        let Some(account) = accounts.get(account_id).cloned() else {
            return Ok(None);
        };
        let (previous_enabled, updated) = {
            let mut config = account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_enabled = config.enabled;
            config.enabled = enabled;
            (previous_enabled, config.clone())
        };
        if let Err(error) = self.persist_accounts(&accounts).await {
            account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .enabled = previous_enabled;
            account.schedule_changed.notify_one();
            return Err(error);
        }
        if !enabled {
            account.state.lock().await.next_probe_at = None;
        }
        account.schedule_changed.notify_one();
        drop(accounts);
        Ok(Some(updated))
    }

    /// Renames an account in place. The provider plus label pair stays unique so
    /// that label selectors keep resolving to exactly one account.
    pub async fn set_account_label(
        &self,
        account_id: &AccountId,
        label: Option<String>,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let accounts = self.inner.accounts.write().await;
        let Some(account) = accounts.get(account_id).cloned() else {
            return Ok(None);
        };
        let provider = account.config().provider;
        if label.is_some()
            && accounts.iter().any(|(id, other)| {
                let other_config = other.config();
                id != account_id
                    && other_config.provider == provider
                    && other_config.query.account_label == label
            })
        {
            return Err(DaemonError::DuplicateAccountSelector {
                provider,
                account_label: label,
            });
        }
        let (previous_label, updated) = {
            let mut config = account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_label = std::mem::replace(&mut config.query.account_label, label);
            (previous_label, config.clone())
        };
        if let Err(error) = self.persist_accounts(&accounts).await {
            account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .query
                .account_label = previous_label;
            return Err(error);
        }
        drop(accounts);
        Ok(Some(updated))
    }

    /// Replaces the display metric names the account's readable summary hides.
    ///
    /// The names are validated by [`MetricFilter`]; a rejected value leaves the
    /// account untouched. A failed persistence rolls the in-memory change back.
    pub async fn set_account_metrics(
        &self,
        account_id: &AccountId,
        metrics: Vec<String>,
    ) -> Result<Option<AccountConfig>, DaemonError> {
        let filter = MetricFilter::new(metrics)
            .map_err(|_| DaemonError::InvalidAccountMetrics(account_id.clone()))?;
        let accounts = self.inner.accounts.write().await;
        let Some(account) = accounts.get(account_id).cloned() else {
            return Ok(None);
        };
        let (previous_metrics, updated) = {
            let mut config = account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_metrics = std::mem::replace(&mut config.metrics, filter.names().to_vec());
            (previous_metrics, config.clone())
        };
        if let Err(error) = self.persist_accounts(&accounts).await {
            account
                .config
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .metrics = previous_metrics;
            return Err(error);
        }
        drop(accounts);
        Ok(Some(updated))
    }
}
