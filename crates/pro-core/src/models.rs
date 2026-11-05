//! The control plane's own records.
//!
//! Three populations that are easy to confuse, so they are named apart
//! throughout:
//!
//! * **Staff** — the operator and whoever they trust. They run the platform.
//!   That is me-the-host, not a customer. See [`StaffRole`].
//! * **Account** — a paying customer of the platform. Owns tenants.
//! * **Tenant** — one Ghost site: a directory, a database, a process, one or
//!   more hostnames.
//!
//! A tenant's *own* users — its authors and editors — are not here at all.
//! They live in that tenant's Ghost database and the platform never reads
//! them. That boundary is the point of the whole design: the control plane
//! knows that a site exists and what plan it is on, and nothing about what is
//! published on it.
use serde::{Deserialize, Serialize};

/// What a member of platform staff may do.
///
/// Ordered, and compared by rank — `Support` is the floor and `Owner` the
/// ceiling — so a check reads `actor.role >= StaffRole::Admin` rather than
/// enumerating every role that passes. A new role added in the middle then
/// does not silently widen an existing check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StaffRole {
    /// Read the console, help a customer, restart a site. Cannot change money
    /// or delete anything.
    Support,
    /// Everything about tenants: plans, suspension, domains, deletion.
    /// Explicitly *not* the payment provider or its credentials.
    Admin,
    /// The platform's owner. The only role that may change how the platform
    /// takes money — which provider is live and the keys behind it — or
    /// promote another member of staff.
    ///
    /// Separated from `Admin` because those two powers are the ones that can
    /// quietly redirect revenue, and that is worth one more deliberate step
    /// even on a platform where the operator is the only person who logs in.
    Owner,
}

impl StaffRole {
    pub fn as_str(self) -> &'static str {
        match self {
            StaffRole::Support => "support",
            StaffRole::Admin => "admin",
            StaffRole::Owner => "owner",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "support" => StaffRole::Support,
            "admin" => StaffRole::Admin,
            "owner" => StaffRole::Owner,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            StaffRole::Support => "Support",
            StaffRole::Admin => "Administrator",
            StaffRole::Owner => "Owner",
        }
    }

    /// Roles an actor may assign, which is everything strictly below their
    /// own plus their own.
    ///
    /// An owner can make another owner; an admin cannot make an owner, which
    /// is what stops the money settings being reachable by promotion.
    pub fn assignable(self) -> Vec<StaffRole> {
        match self {
            StaffRole::Owner => vec![StaffRole::Support, StaffRole::Admin, StaffRole::Owner],
            StaffRole::Admin => vec![StaffRole::Support],
            StaffRole::Support => vec![],
        }
    }
}

/// A member of platform staff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Staff {
    pub id: String,
    pub email: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: StaffRole,
    pub status: String,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

impl Staff {
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
    pub fn can(&self, needed: StaffRole) -> bool {
        self.is_active() && self.role >= needed
    }
}

/// A paying customer of the platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    /// `active`, `blocked`.
    pub status: String,
    pub email_verified_at: Option<String>,
    /// The customer id at whichever provider this account pays through, and
    /// which provider that is. Stored as a pair because an account created
    /// under one provider keeps paying there after the platform's default
    /// moves on — so "which provider is this customer on" is a property of the
    /// account, not of the platform.
    pub billing_provider: Option<String>,
    pub billing_customer_id: Option<String>,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

impl Account {
    pub fn is_verified(&self) -> bool {
        self.email_verified_at.is_some()
    }
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// Where a tenant is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    /// Directories and database are being created. Not yet routable.
    Provisioning,
    /// Normal.
    Active,
    /// Online but closed to the public: billing lapsed, or an operator shut
    /// it. The admin still answers so the owner can pay and come back.
    Suspended,
    /// Scheduled for deletion, still recoverable. The grace period before the
    /// files actually go.
    PendingDeletion,
    /// Files are gone. The row stays so the slug is not immediately reissued
    /// and so an audit trail survives.
    Deleted,
    /// Provisioning failed. Kept rather than discarded, because the half-built
    /// directory needs cleaning up and an operator needs to see why.
    Failed,
}

impl TenantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TenantStatus::Provisioning => "provisioning",
            TenantStatus::Active => "active",
            TenantStatus::Suspended => "suspended",
            TenantStatus::PendingDeletion => "pending_deletion",
            TenantStatus::Deleted => "deleted",
            TenantStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "provisioning" => TenantStatus::Provisioning,
            "active" => TenantStatus::Active,
            "suspended" => TenantStatus::Suspended,
            "pending_deletion" => TenantStatus::PendingDeletion,
            "deleted" => TenantStatus::Deleted,
            "failed" => TenantStatus::Failed,
            _ => return None,
        })
    }

    /// Whether the edge should route traffic to this tenant at all.
    ///
    /// Suspended is routable on purpose: the site answers, and its own
    /// `publicSiteAccess` limit is what closes the public half while leaving
    /// the admin reachable. Doing it that way rather than refusing at the edge
    /// is what lets a lapsed customer still sign in and pay.
    pub fn is_routable(self) -> bool {
        matches!(self, TenantStatus::Active | TenantStatus::Suspended | TenantStatus::PendingDeletion)
    }

    pub fn label(self) -> &'static str {
        match self {
            TenantStatus::Provisioning => "Setting up",
            TenantStatus::Active => "Active",
            TenantStatus::Suspended => "Suspended",
            TenantStatus::PendingDeletion => "Pending deletion",
            TenantStatus::Deleted => "Deleted",
            TenantStatus::Failed => "Failed",
        }
    }
}

/// One Ghost site.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: String,
    /// The subdomain label, and the tenant's stable public name.
    pub slug: String,
    /// The site's title, as shown in the dashboard. Ghost holds its own copy
    /// in the tenant's settings; this one is for the platform's lists.
    pub name: String,
    pub account_id: String,
    pub plan_slug: String,
    pub status: TenantStatus,
    /// Loopback port the tenant process listens on. Assigned once and kept for
    /// the tenant's life, so a restart does not move it.
    pub port: u16,
    /// ISO-8601, the date periodic limits are counted from. Normally the
    /// subscription's start.
    pub period_start: String,
    pub created_at: String,
    pub updated_at: String,
    /// Last time the edge forwarded a request here. Drives hibernation.
    pub last_request_at: Option<String>,
    pub suspended_at: Option<String>,
    pub suspended_reason: Option<String>,
    /// When the files may actually be removed.
    pub delete_after: Option<String>,
    /// Bytes of content on disk, as last measured. Checked against the plan's
    /// storage quota, which Ghost knows nothing about.
    pub storage_bytes: i64,
    pub notes: Option<String>,
    /// The admin API key of the platform's integration on this site: the id
    /// (`kid`) and the secret as hex. `None` on sites created before the
    /// platform learned to keep one, or whose owner revoked it.
    pub platform_key_id: Option<String>,
    pub platform_key_secret: Option<String>,
}

impl Tenant {
    /// The hostname the platform issued, which is always routable even after a
    /// custom domain is attached — so a tenant is never unreachable because
    /// its DNS broke.
    pub fn platform_host(&self, subdomain_base: &str) -> String {
        format!("{}.{}", self.slug, subdomain_base.trim_matches('.'))
    }
}

/// How a hostname got here, which decides what has to be true before it is
/// served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainKind {
    /// `slug.platform.com`. The platform controls the DNS, so it is live
    /// immediately and cannot be taken away.
    Subdomain,
    /// A hostname the customer owns. Must be proved and pointed here first.
    Custom,
}

impl DomainKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DomainKind::Subdomain => "subdomain",
            DomainKind::Custom => "custom",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "subdomain" => DomainKind::Subdomain,
            "custom" => DomainKind::Custom,
            _ => return None,
        })
    }
}

/// How far along a custom domain is.
///
/// The states exist because attaching a domain is a conversation with the
/// customer's DNS provider that the platform cannot hurry, and each state is a
/// different sentence to show them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainStatus {
    /// Added, DNS not yet pointing here. The customer has instructions to
    /// follow.
    Pending,
    /// DNS resolves to this platform. A certificate is being obtained.
    Verified,
    /// Certificate issued, traffic served.
    Active,
    /// Verification has been failing long enough to be worth saying so.
    /// Retried regardless — DNS gets fixed without anybody telling us.
    Failed,
}

impl DomainStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DomainStatus::Pending => "pending",
            DomainStatus::Verified => "verified",
            DomainStatus::Active => "active",
            DomainStatus::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => DomainStatus::Pending,
            "verified" => DomainStatus::Verified,
            "active" => DomainStatus::Active,
            "failed" => DomainStatus::Failed,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            DomainStatus::Pending => "Waiting for DNS",
            DomainStatus::Verified => "Issuing certificate",
            DomainStatus::Active => "Live",
            DomainStatus::Failed => "DNS not found",
        }
    }
}

/// A hostname that routes to a tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Domain {
    pub id: String,
    pub tenant_id: String,
    /// Lowercase, no port, no trailing dot.
    pub domain: String,
    pub kind: DomainKind,
    pub status: DomainStatus,
    /// The canonical hostname: the one the tenant's Ghost `url` is set to, and
    /// therefore the one every link and every redirect uses. Exactly one per
    /// tenant.
    pub is_primary: bool,
    /// Token the customer may publish as a TXT record to prove ownership,
    /// for the case where they cannot point an A record yet.
    pub verification_token: String,
    pub verified_at: Option<String>,
    pub last_checked_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
}

/// A platform subscription — what the customer pays *us*.
///
/// Not to be confused with the member subscriptions inside a tenant's own
/// Ghost, which are that site's business and live in its database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub account_id: String,
    /// The tenant this pays for. A subscription is per-site, as Ghost Pro's
    /// is, so one account can hold several on different plans.
    pub tenant_id: Option<String>,
    pub plan_slug: String,
    /// Which provider holds this subscription: `stripe`, `paddle`,
    /// `lemonsqueezy`, `gumroad`, or `manual` for one an operator granted.
    pub provider: String,
    /// The subscription's id at that provider.
    pub provider_subscription_id: Option<String>,
    pub provider_customer_id: Option<String>,
    /// `trialing`, `active`, `past_due`, `paused`, `canceled`, `expired`.
    /// Normalised across providers, which all name these differently.
    pub status: String,
    pub current_period_start: Option<String>,
    pub current_period_end: Option<String>,
    pub trial_ends_at: Option<String>,
    pub cancel_at_period_end: bool,
    pub canceled_at: Option<String>,
    /// When dunning started, so the grace period can be measured from it.
    pub past_due_since: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Subscription {
    /// Whether this subscription entitles the tenant to run normally.
    ///
    /// `past_due` counts: that is what the grace period is for. The platform
    /// suspends when the grace period expires, not when the first payment
    /// fails, because the commonest cause is an expired card and the
    /// commonest fix takes a day.
    pub fn is_entitled(&self) -> bool {
        matches!(self.status.as_str(), "trialing" | "active" | "past_due")
    }

    pub fn is_trialing(&self) -> bool {
        self.status == "trialing"
    }
}

/// An entry in the audit trail.
///
/// Written for anything that changes money, routing or a tenant's existence.
/// The platform is something customers trust with their publication, so "who
/// suspended this site, and when" has to be answerable later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    /// `staff:<id>`, `account:<id>`, `system`, or `webhook:<provider>`.
    pub actor: String,
    /// `tenant.created`, `tenant.suspended`, `settings.changed`,
    /// `domain.added`, `subscription.updated`.
    pub action: String,
    pub tenant_id: Option<String>,
    pub account_id: Option<String>,
    /// Whatever is worth keeping about this particular event.
    pub meta: serde_json::Value,
    pub created_at: String,
}
