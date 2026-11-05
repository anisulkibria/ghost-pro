//! Plans, and the `hostSettings` block each one compiles into.
//!
//! This is the whole contract between the platform and a tenant. Ghost-rust
//! already enforces host limits — `ghost_db::limits::Limits::load` reads
//! `hostSettings.limits` out of the site's config and every create path checks
//! against it. So the platform does not need to police tenants at the edge: it
//! only has to write the right JSON into the right config file, and the site
//! enforces its own plan from the inside.
//!
//! That is why the limit *names* here are not ours to choose. They are the
//! names Ghost's `limit-service` knows, and anything else is dropped with a
//! warning by the site at boot. The set is:
//!
//! * counted ceilings — `members`, `staff`, `newsletters`, `limitAutomations`,
//!   `uploads`
//! * per-period ceilings — `emails`, dated from `subscription.start`
//! * flags — `customIntegrations`, `customThemes`, `limitStripeConnect`,
//!   `limitAnalytics`, `limitSocialWeb`, `limitCustomFields`,
//!   `publicSiteAccess`
//! * allowlists — `customThemes` with a list of permitted theme names
//!
//! `customThemes` appears twice on purpose: as a flag it means "no themes but
//! the official ones", and as an allowlist it means "these themes only". The
//! site reads an allowlist in preference to a flag, which is what lets a plan
//! say either.
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// A plan as the platform sells it.
///
/// Prices live here as integer minor units (cents) to keep money out of
/// floating point. These are the numbers the pricing page shows; the payment
/// provider is the source of truth for what was actually charged, and
/// `price_ids` is the link between the two.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Stable identifier used in the database and in URLs: `starter`,
    /// `creator`, `team`, `business`.
    pub slug: String,
    pub name: String,
    /// Shown on the pricing page.
    pub description: String,
    pub price_cents: i64,
    /// `month` or `year`.
    pub interval: String,
    pub currency: String,
    /// What this plan is called at each payment provider, keyed by provider
    /// slug: `stripe` → a price id (`price_…`), `paddle` → a price id
    /// (`pri_…`), `lemonsqueezy` → a variant id (`"812345"`).
    ///
    /// A map rather than three fields because the platform can be pointed at
    /// any one of the three — and an operator who migrates between them wants
    /// both ids live at once during the changeover, with existing
    /// subscriptions still billing at the old provider while new signups go to
    /// the new one. A plan with no id for the *active* provider is not for
    /// sale and does not appear on the pricing page, which is also how an
    /// internal plan an operator assigns by hand stays unlisted.
    #[serde(default)]
    pub price_ids: BTreeMap<String, String>,

    // ---- what the plan includes -------------------------------------------
    /// Ceiling on members. `None` is unlimited.
    #[serde(default)]
    pub max_members: Option<i64>,
    /// Ceiling on staff users. Contributors are free in Ghost's own counting,
    /// so this is editors and above plus pending invites.
    #[serde(default)]
    pub max_staff: Option<i64>,
    /// Ceiling on active newsletters.
    #[serde(default)]
    pub max_newsletters: Option<i64>,
    /// Ceiling on active automations.
    #[serde(default)]
    pub max_automations: Option<i64>,
    /// Emails per billing period, counted from the subscription's start date.
    #[serde(default)]
    pub max_emails_per_period: Option<i64>,
    /// Largest single upload, in bytes.
    #[serde(default)]
    pub max_upload_bytes: Option<i64>,

    /// May the tenant install themes of its own? `false` restricts it to the
    /// official set.
    #[serde(default)]
    pub custom_themes: bool,
    /// If set, the only themes that may be installed — a stricter form of
    /// `custom_themes` for plans that ship a curated list.
    #[serde(default)]
    pub theme_allowlist: Vec<String>,
    /// Custom integrations and the Admin API keys that come with them.
    #[serde(default)]
    pub custom_integrations: bool,
    /// May the tenant take paid member subscriptions at all?
    ///
    /// Named for the capability rather than the processor on purpose. Today
    /// the CMS has exactly one way to charge a member — Stripe Connect — so
    /// this compiles to Ghost's `limitStripeConnect` flag and nothing else.
    /// When the CMS gains further processors, this plan field does not change:
    /// only [`Plan::host_settings`] does, emitting whichever limit names the
    /// CMS has by then. That keeps a stored plan, and every operator's
    /// `pro.json`, valid across that change.
    ///
    /// `stripe_connect` stays accepted as an alias so configs written against
    /// the earlier name keep loading.
    #[serde(default, alias = "stripe_connect", alias = "stripeConnect")]
    pub paid_memberships: bool,
    /// The analytics screens.
    #[serde(default)]
    pub analytics: bool,
    /// Social web / ActivityPub federation.
    #[serde(default)]
    pub social_web: bool,
    /// Custom fields on posts.
    #[serde(default)]
    pub custom_fields: bool,
    /// Whether the site is readable by the public at all. Turned off is how a
    /// suspended or unpaid site is kept online but closed — the admin still
    /// works, so the owner can pay and come back.
    #[serde(default = "yes")]
    pub public_site_access: bool,

    // ---- platform-side allowances -----------------------------------------
    /// How many custom domains the tenant may attach. Ghost knows nothing
    /// about this one; it is enforced here, because domains are the platform's
    /// to route.
    #[serde(default)]
    pub max_custom_domains: i64,
    /// Storage for content (images, media, files), in bytes. Enforced by the
    /// platform's own accounting, since Ghost has no notion of a disk quota.
    #[serde(default)]
    pub max_storage_bytes: Option<i64>,
    /// Days of trial granted on signup. `0` means payment up front.
    #[serde(default)]
    pub trial_days: i64,
    /// Order on the pricing page; lower is first.
    #[serde(default)]
    pub sort_order: i64,
    /// Hidden plans still work for accounts already on them, but are not
    /// offered to anybody new — which is how a price rise leaves existing
    /// customers alone.
    #[serde(default)]
    pub hidden: bool,
}

fn yes() -> bool {
    true
}

impl Plan {
    /// Compile this plan into the `hostSettings` object a tenant's
    /// `config.production.json` carries.
    ///
    /// `subscription_start` must be an ISO-8601 date: the periodic email limit
    /// is dated from it, and a site whose config has a `maxPeriodic` limit but
    /// no `subscription.start` drops that limit at boot with a warning. So an
    /// email ceiling only takes effect once the platform knows when the
    /// billing period began — which is exactly the right behaviour, but it
    /// means this argument is not optional in practice.
    ///
    /// `billing_url` is where the site's own admin screens send an owner who
    /// hits a limit. Ghost falls back to `https://ghost.org/help/` when the
    /// host supplies none, which would be wrong here — on this platform the
    /// owner's billing page is ours, not Ghost's.
    pub fn host_settings(&self, subscription_start: &str, billing_url: &str) -> Value {
        let mut limits = Map::new();

        // Counted ceilings. A plan that leaves one unset is unlimited for it,
        // and the key is omitted entirely rather than set high — an absent
        // limit costs the site one map lookup, where a present one costs a
        // COUNT(*) on every create.
        if let Some(max) = self.max_members {
            limits.insert(
                "members".into(),
                json!({
                    "max": max,
                    "error": format!(
                        "Your {} plan includes {{{{max}}}} members. You currently have {{{{count}}}}. \
                         Upgrade to add more.",
                        self.name
                    ),
                }),
            );
        }
        if let Some(max) = self.max_staff {
            limits.insert(
                "staff".into(),
                json!({
                    "max": max,
                    "error": format!(
                        "Your {} plan includes {{{{max}}}} staff users. Upgrade to invite more.",
                        self.name
                    ),
                }),
            );
        }
        if let Some(max) = self.max_newsletters {
            limits.insert(
                "newsletters".into(),
                json!({
                    "max": max,
                    "error": format!("Your {} plan includes {{{{max}}}} newsletters.", self.name),
                }),
            );
        }
        if let Some(max) = self.max_automations {
            limits.insert(
                "limitAutomations".into(),
                json!({
                    "max": max,
                    "error": format!("Your {} plan includes {{{{max}}}} active automations.", self.name),
                }),
            );
        }
        if let Some(max) = self.max_upload_bytes {
            limits.insert(
                "uploads".into(),
                json!({
                    "max": max,
                    "error": "This file is larger than your plan allows.",
                }),
            );
        }

        // The one periodic limit. Dated from the subscription anniversary, so
        // a tenant's email allowance resets on their own billing day rather
        // than on the first of the month.
        if let Some(max) = self.max_emails_per_period {
            limits.insert(
                "emails".into(),
                json!({
                    "maxPeriodic": max,
                    "error": format!(
                        "Your {} plan includes {{{{max}}}} emails per month. You have sent {{{{count}}}}.",
                        self.name
                    ),
                }),
            );
        }

        // Flags. These are written only when the plan *withholds* the feature:
        // `disabled: false` and an absent key mean the same thing to the site,
        // and leaving it absent keeps the config readable — what is listed is
        // what is restricted.
        let mut flag = |name: &str, included: bool, message: &str| {
            if !included {
                limits.insert(name.into(), json!({"disabled": true, "error": message}));
            }
        };
        flag(
            "limitStripeConnect",
            self.paid_memberships,
            "Connecting Stripe to take payments is available on a higher plan.",
        );
        flag("limitAnalytics", self.analytics, "Analytics are available on a higher plan.");
        flag("limitSocialWeb", self.social_web, "The social web is available on a higher plan.");
        flag("limitCustomFields", self.custom_fields, "Custom fields are available on a higher plan.");
        flag(
            "customIntegrations",
            self.custom_integrations,
            "Custom integrations are available on a higher plan.",
        );
        flag(
            "publicSiteAccess",
            self.public_site_access,
            "This site is not currently published.",
        );

        // Themes: an allowlist if the plan names one, otherwise a flag. The
        // site prefers the allowlist when both are present, so writing only
        // one of the two keeps the intent unambiguous.
        if !self.theme_allowlist.is_empty() {
            limits.insert(
                "customThemes".into(),
                json!({
                    "allowlist": self.theme_allowlist,
                    "error": "This theme is not available on your plan.",
                }),
            );
        } else if !self.custom_themes {
            limits.insert(
                "customThemes".into(),
                json!({
                    "disabled": true,
                    "error": "Uploading your own theme is available on a higher plan.",
                }),
            );
        }

        json!({
            "siteId": Value::Null,
            "billing": { "enabled": true, "url": billing_url },
            "subscription": { "start": subscription_start },
            "limits": Value::Object(limits),
            // Named so an operator reading a tenant's config can see which
            // plan produced these numbers without consulting the database.
            "plan": { "slug": self.slug, "name": self.name },
        })
    }

    /// What this plan is called at `provider`, if it is sold there.
    pub fn price_id(&self, provider: &str) -> Option<&str> {
        self.price_ids.get(provider).map(String::as_str).filter(|s| !s.trim().is_empty())
    }

    /// Whether a customer can buy this plan through `provider` right now.
    ///
    /// A hidden plan stays usable for accounts already on it — this only
    /// governs whether it is offered to somebody new.
    pub fn is_purchasable(&self, provider: &str) -> bool {
        !self.hidden && self.price_id(provider).is_some()
    }

    /// Price formatted for display: `$12` or `$12.50`.
    pub fn price_display(&self) -> String {
        let symbol = match self.currency.to_uppercase().as_str() {
            "USD" => "$",
            "EUR" => "€",
            "GBP" => "£",
            other => return format!("{:.2} {other}", self.price_cents as f64 / 100.0),
        };
        if self.price_cents % 100 == 0 {
            format!("{symbol}{}", self.price_cents / 100)
        } else {
            format!("{symbol}{:.2}", self.price_cents as f64 / 100.0)
        }
    }

    /// Human-readable feature bullets for a pricing page.
    pub fn features(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(match self.max_members {
            Some(n) => format!("{} members", thousands(n)),
            None => "Unlimited members".into(),
        });
        out.push(match self.max_staff {
            Some(1) => "1 staff user".into(),
            Some(n) => format!("{n} staff users"),
            None => "Unlimited staff users".into(),
        });
        if let Some(n) = self.max_emails_per_period {
            out.push(format!("{} emails / month", thousands(n)));
        }
        out.push(match self.max_custom_domains {
            0 => "Subdomain only".into(),
            1 => "Custom domain".into(),
            n => format!("{n} custom domains"),
        });
        if let Some(bytes) = self.max_storage_bytes {
            out.push(format!("{} storage", bytes_display(bytes)));
        }
        if self.paid_memberships {
            out.push("Paid memberships via Stripe".into());
        }
        if self.custom_themes || !self.theme_allowlist.is_empty() {
            out.push("Custom themes".into());
        }
        if self.custom_integrations {
            out.push("Custom integrations & API access".into());
        }
        if self.analytics {
            out.push("Analytics".into());
        }
        out
    }
}

fn thousands(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Bytes as the nearest sensible unit. Used for quotas and backup sizes.
pub fn bytes_display(bytes: i64) -> String {
    const UNITS: [(&str, i64); 4] =
        [("TB", 1 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (unit, size) in UNITS {
        if bytes >= size {
            let v = bytes as f64 / size as f64;
            return if v >= 10.0 {
                format!("{:.0} {unit}", v)
            } else {
                format!("{:.1} {unit}", v)
            };
        }
    }
    format!("{bytes} B")
}

/// The plans the platform ships with.
///
/// Deliberately modelled on Ghost Pro's own tiers so the shape is familiar,
/// but these are defaults an operator is expected to edit: prices, ceilings
/// and Stripe price ids are all theirs. `pro.json` overrides this list
/// wholesale when it carries a `plans` array, so changing the catalogue does
/// not mean changing this file.
pub fn default_plans() -> Vec<Plan> {
    vec![
        Plan {
            slug: "starter".into(),
            name: "Starter".into(),
            description: "A publication for one person, on your own domain.".into(),
            price_cents: 900,
            interval: "month".into(),
            currency: "USD".into(),
            price_ids: BTreeMap::new(),
            max_members: Some(500),
            max_staff: Some(1),
            max_newsletters: Some(1),
            max_automations: Some(3),
            max_emails_per_period: Some(10_000),
            max_upload_bytes: Some(10 << 20),
            custom_themes: false,
            theme_allowlist: vec![],
            custom_integrations: false,
            paid_memberships: true,
            analytics: true,
            social_web: false,
            custom_fields: false,
            public_site_access: true,
            max_custom_domains: 1,
            max_storage_bytes: Some(2 << 30),
            trial_days: 14,
            sort_order: 1,
            hidden: false,
        },
        Plan {
            slug: "creator".into(),
            name: "Creator".into(),
            description: "Custom themes, integrations, and a small team.".into(),
            price_cents: 2500,
            interval: "month".into(),
            currency: "USD".into(),
            price_ids: BTreeMap::new(),
            max_members: Some(1_000),
            max_staff: Some(2),
            max_newsletters: Some(3),
            max_automations: Some(10),
            max_emails_per_period: Some(50_000),
            max_upload_bytes: Some(50 << 20),
            custom_themes: true,
            theme_allowlist: vec![],
            custom_integrations: true,
            paid_memberships: true,
            analytics: true,
            social_web: true,
            custom_fields: true,
            public_site_access: true,
            max_custom_domains: 2,
            max_storage_bytes: Some(10 << 30),
            trial_days: 14,
            sort_order: 2,
            hidden: false,
        },
        Plan {
            slug: "team".into(),
            name: "Team".into(),
            description: "For publications with a newsroom behind them.".into(),
            price_cents: 5000,
            interval: "month".into(),
            currency: "USD".into(),
            price_ids: BTreeMap::new(),
            max_members: Some(10_000),
            max_staff: Some(5),
            max_newsletters: Some(10),
            max_automations: None,
            max_emails_per_period: Some(250_000),
            max_upload_bytes: Some(100 << 20),
            custom_themes: true,
            theme_allowlist: vec![],
            custom_integrations: true,
            paid_memberships: true,
            analytics: true,
            social_web: true,
            custom_fields: true,
            public_site_access: true,
            max_custom_domains: 5,
            max_storage_bytes: Some(50 << 30),
            trial_days: 14,
            sort_order: 3,
            hidden: false,
        },
        Plan {
            slug: "business".into(),
            name: "Business".into(),
            description: "Unlimited everything, with priority support.".into(),
            price_cents: 19900,
            interval: "month".into(),
            currency: "USD".into(),
            price_ids: BTreeMap::new(),
            max_members: None,
            max_staff: None,
            max_newsletters: None,
            max_automations: None,
            max_emails_per_period: Some(1_000_000),
            max_upload_bytes: Some(1 << 30),
            custom_themes: true,
            theme_allowlist: vec![],
            custom_integrations: true,
            paid_memberships: true,
            analytics: true,
            social_web: true,
            custom_fields: true,
            public_site_access: true,
            max_custom_domains: 25,
            max_storage_bytes: None,
            trial_days: 0,
            sort_order: 4,
            hidden: false,
        },
    ]
}

/// The plan a site is put on when its subscription lapses.
///
/// Not a plan anybody buys: it keeps the admin reachable so the owner can fix
/// their billing, while `publicSiteAccess` closes the public site and every
/// ceiling drops to what already exists. The site stays whole — nothing is
/// deleted — which is the difference between suspended and cancelled.
pub fn suspended_plan() -> Plan {
    Plan {
        slug: "suspended".into(),
        name: "Suspended".into(),
        description: "Billing needs attention.".into(),
        price_cents: 0,
        interval: "month".into(),
        currency: "USD".into(),
        price_ids: BTreeMap::new(),
        max_members: Some(0),
        max_staff: None,
        max_newsletters: Some(0),
        max_automations: Some(0),
        max_emails_per_period: Some(0),
        max_upload_bytes: Some(0),
        custom_themes: false,
        theme_allowlist: vec![],
        custom_integrations: false,
        paid_memberships: false,
        analytics: false,
        social_web: false,
        custom_fields: false,
        // The point of the whole plan.
        public_site_access: false,
        max_custom_domains: 0,
        max_storage_bytes: Some(0),
        trial_days: 0,
        sort_order: 99,
        hidden: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names written into `hostSettings.limits` have to be ones Ghost's
    /// limit service knows; anything else is dropped at boot with a warning,
    /// which would turn a paid ceiling into no ceiling at all. This is the
    /// list from `ghost_db::limits`.
    #[test]
    fn only_emits_limit_names_ghost_understands() {
        const KNOWN: &[&str] = &[
            "members",
            "staff",
            "newsletters",
            "limitAutomations",
            "emails",
            "uploads",
            "customIntegrations",
            "customThemes",
            "limitStripeConnect",
            "limitAnalytics",
            "limitSocialWeb",
            "limitCustomFields",
            "publicSiteAccess",
        ];
        for plan in default_plans().into_iter().chain([suspended_plan()]) {
            let hs = plan.host_settings("2026-01-01T00:00:00Z", "https://pro.example/billing");
            for name in hs["limits"].as_object().unwrap().keys() {
                assert!(KNOWN.contains(&name.as_str()), "plan {} emitted unknown limit {name}", plan.slug);
            }
        }
    }

    /// A periodic limit with no `subscription.start` is discarded by the site,
    /// so the two have to travel together.
    #[test]
    fn periodic_limit_ships_with_a_start_date() {
        let plan = &default_plans()[0];
        let hs = plan.host_settings("2026-03-04T00:00:00Z", "https://pro.example/billing");
        assert!(hs["limits"]["emails"]["maxPeriodic"].is_i64());
        assert_eq!(hs["subscription"]["start"], "2026-03-04T00:00:00Z");
    }

    /// An unlimited plan must omit the key rather than send a large number —
    /// a present limit costs a COUNT(*) on every create.
    #[test]
    fn unlimited_omits_the_limit() {
        let business = default_plans().into_iter().find(|p| p.slug == "business").unwrap();
        let hs = business.host_settings("2026-01-01T00:00:00Z", "https://pro.example/billing");
        let limits = hs["limits"].as_object().unwrap();
        assert!(!limits.contains_key("members"));
        assert!(!limits.contains_key("staff"));
    }

    /// Features the plan grants must not appear as restrictions.
    #[test]
    fn granted_features_are_not_written_as_flags() {
        let creator = default_plans().into_iter().find(|p| p.slug == "creator").unwrap();
        let hs = creator.host_settings("2026-01-01T00:00:00Z", "https://pro.example/billing");
        let limits = hs["limits"].as_object().unwrap();
        for granted in ["limitStripeConnect", "limitAnalytics", "customIntegrations", "customThemes"] {
            assert!(!limits.contains_key(granted), "{granted} should be absent on creator");
        }
    }

    /// Starter withholds custom themes, so the flag has to be present and on.
    #[test]
    fn withheld_features_are_disabled() {
        let starter = default_plans().into_iter().find(|p| p.slug == "starter").unwrap();
        let hs = starter.host_settings("2026-01-01T00:00:00Z", "https://pro.example/billing");
        assert_eq!(hs["limits"]["customThemes"]["disabled"], true);
        assert_eq!(hs["limits"]["limitSocialWeb"]["disabled"], true);
    }

    /// An allowlist and a flag both claim `customThemes`; only one may be
    /// written, because the site silently prefers the allowlist.
    #[test]
    fn theme_allowlist_replaces_the_flag() {
        let mut plan = default_plans().into_iter().next().unwrap();
        plan.theme_allowlist = vec!["casper".into(), "source".into()];
        let hs = plan.host_settings("2026-01-01T00:00:00Z", "https://pro.example/billing");
        assert!(hs["limits"]["customThemes"]["allowlist"].is_array());
        assert!(hs["limits"]["customThemes"]["disabled"].is_null());
    }

    /// Suspension closes the public site and nothing else about it is
    /// destructive — the staff ceiling stays open so the owner can still sign
    /// in and pay.
    #[test]
    fn suspension_closes_the_site_but_keeps_the_admin() {
        let hs = suspended_plan().host_settings("2026-01-01T00:00:00Z", "https://pro.example/billing");
        assert_eq!(hs["limits"]["publicSiteAccess"]["disabled"], true);
        assert!(hs["limits"]["staff"].is_null());
    }

    /// The billing link has to be the platform's, not ghost.org's — the site
    /// falls back to Ghost's help page when the host sends none.
    #[test]
    fn billing_url_is_the_platforms() {
        let hs = default_plans()[0].host_settings("2026-01-01T00:00:00Z", "https://pro.example/billing");
        assert_eq!(hs["billing"]["enabled"], true);
        assert_eq!(hs["billing"]["url"], "https://pro.example/billing");
    }

    /// A plan is only for sale where it has an id, so the same catalogue can
    /// be live at two providers during a migration without offering a plan
    /// neither of them can charge for.
    #[test]
    fn purchasable_only_where_a_price_id_exists() {
        let mut plan = default_plans().into_iter().next().unwrap();
        assert!(!plan.is_purchasable("stripe"), "no ids configured yet");

        plan.price_ids.insert("stripe".into(), "price_123".into());
        plan.price_ids.insert("lemonsqueezy".into(), "812345".into());
        assert_eq!(plan.price_id("stripe"), Some("price_123"));
        assert_eq!(plan.price_id("lemonsqueezy"), Some("812345"));
        assert!(plan.is_purchasable("stripe"));
        assert!(plan.is_purchasable("lemonsqueezy"));
        // Not sold at Paddle, so a Paddle-configured platform must not list it.
        assert!(!plan.is_purchasable("paddle"));

        // A blank id is a half-finished config, not a price.
        plan.price_ids.insert("paddle".into(), "  ".into());
        assert!(!plan.is_purchasable("paddle"));

        plan.hidden = true;
        assert!(!plan.is_purchasable("stripe"), "hidden plans are not offered");
    }

    #[test]
    fn formats_money_and_bytes() {
        let p = &default_plans()[0];
        assert_eq!(p.price_display(), "$9");
        assert_eq!(bytes_display(2 << 30), "2.0 GB");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
