//! Columns of the `contacts` table (also joined as `current_contact_*` in `log`).

use std::sync::Arc;

use corax_core::Contact;

use super::Ctx;
use crate::labels;
use crate::value::Value;

pub type ContactFn = Arc<dyn for<'x> Fn(&Ctx<'x>, &Contact) -> Value + Send + Sync>;

fn f<F>(f: F) -> Option<ContactFn>
where
    F: for<'x> Fn(&Ctx<'x>, &Contact) -> Value + Send + Sync + 'static,
{
    Some(Arc::new(f))
}

pub fn column(name: &str) -> Option<ContactFn> {
    if let Some(n) = name.strip_prefix("address") {
        let idx: usize = n.parse().ok().filter(|i| (1..=6).contains(i))?;
        return f(move |_, c| Value::Str(c.cfg.addresses.get(idx - 1).cloned().unwrap_or_default()));
    }
    match name {
        "name" => f(|_, c| Value::str(&c.cfg.name)),
        "alias" => f(|_, c| Value::str(&c.cfg.alias)),
        "email" => f(|_, c| Value::str(&c.cfg.email)),
        "pager" => f(|_, c| Value::str(&c.cfg.pager)),
        "can_submit_commands" => f(|_, c| Value::bool(c.cfg.can_submit_commands)),
        "host_notifications_enabled" => f(|_, c| Value::bool(c.host_notifications_enabled)),
        "service_notifications_enabled" => f(|_, c| Value::bool(c.service_notifications_enabled)),
        "host_notification_period" => f(|_, c| Value::Str(c.cfg.host_notification_period.clone().unwrap_or_default())),
        "service_notification_period" => {
            f(|_, c| Value::Str(c.cfg.service_notification_period.clone().unwrap_or_default()))
        }
        "in_host_notification_period" => {
            f(|x, c| Value::bool(x.timeperiod_active(c.cfg.host_notification_period.as_deref())))
        }
        "in_service_notification_period" => {
            f(|x, c| Value::bool(x.timeperiod_active(c.cfg.service_notification_period.as_deref())))
        }
        "modified_attributes" => f(|_, c| Value::Int(c.modified_attributes as i64)),
        "modified_attributes_list" => f(|_, c| {
            Value::str_list(
                corax_core::model::modattr::NAMES
                    .iter()
                    .filter(|(b, _)| c.modified_attributes & b != 0)
                    .map(|(_, n)| *n),
            )
        }),
        "custom_variable_names" => {
            f(|_, c| Value::str_list(labels::custom_only(&c.cfg.custom_vars).map(|x| x.0.clone())))
        }
        "custom_variable_values" => {
            f(|_, c| Value::str_list(labels::custom_only(&c.cfg.custom_vars).map(|x| x.1.clone())))
        }
        "custom_variables" => {
            f(|_, c| Value::str_dict(labels::custom_only(&c.cfg.custom_vars).map(|(k, v)| (k.clone(), v.clone()))))
        }
        "labels" => f(|_, c| Value::str_dict(labels::split(&c.cfg.custom_vars).labels)),
        "label_names" => f(|_, c| Value::str_list(labels::split(&c.cfg.custom_vars).labels.into_iter().map(|x| x.0))),
        "label_values" => f(|_, c| Value::str_list(labels::split(&c.cfg.custom_vars).labels.into_iter().map(|x| x.1))),
        "label_sources" => f(|_, c| Value::str_dict(labels::split(&c.cfg.custom_vars).label_sources)),
        "tags" => f(|_, c| Value::str_dict(labels::split(&c.cfg.custom_vars).tags)),
        _ => None,
    }
}
