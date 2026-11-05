//! Platform staff: the operator's own logins.
use pro_core::{ids, looks_like_email, now_iso, DbErrorExt, ProError, Result, Staff, StaffRole};
use crate::Db;
use sqlx::any::AnyRow;
use sqlx::Row;

use crate::accounts::{hash_password, verify_password};

fn map(row: &AnyRow) -> Staff {
    Staff {
        id: row.get("id"),
        email: row.get("email"),
        name: row.get("name"),
        password_hash: row.get("password_hash"),
        // An unreadable role is read as the *least* privileged one. Defaulting
        // the other way — or panicking — would turn a corrupt row or a
        // downgrade into either an accidental owner or an unusable console.
        role: StaffRole::parse(row.get::<String, _>("role").as_str()).unwrap_or(StaffRole::Support),
        status: row.get("status"),
        created_at: row.get("created_at"),
        last_login_at: row.get("last_login_at"),
    }
}

const COLS: &str =
    "id, email, name, password_hash, role, status, created_at, last_login_at";

pub async fn create(
    pool: &Db,
    email: &str,
    name: &str,
    password: &str,
    role: StaffRole,
) -> Result<Staff> {
    let email = email.trim().to_lowercase();
    if !looks_like_email(&email) {
        return Err(ProError::BadRequest("That does not look like an email address.".into()));
    }
    if password.len() < 12 {
        // Longer than a customer's minimum on purpose: these logins can
        // suspend every site on the platform or redirect its revenue.
        return Err(ProError::BadRequest(
            "A staff password must be at least 12 characters.".into(),
        ));
    }
    let id = ids::object_id();
    let result = sqlx::query(
        "INSERT INTO staff (id, email, name, password_hash, role, status, created_at) \
         VALUES (?, ?, ?, ?, ?, 'active', ?)",
    )
    .bind(&id)
    .bind(&email)
    .bind(name.trim())
    .bind(hash_password(password)?)
    .bind(role.as_str())
    .bind(now_iso())
    .execute(pool)
    .await;

    if let Err(e) = result {
        let msg = e.to_string();
        if crate::unique::is_unique_violation(&msg, &["staff_email_unique", "staff.email"]) {
            return Err(ProError::Conflict("A staff member with that email already exists.".into()));
        }
        return Err(ProError::Db(msg));
    }
    get(pool, &id).await
}

pub async fn get(pool: &Db, id: &str) -> Result<Staff> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM staff WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .db_err()?
        .ok_or_else(|| ProError::NotFound(format!("no staff member {id}")))?;
    Ok(map(&row))
}

pub async fn find_by_email(pool: &Db, email: &str) -> Result<Option<Staff>> {
    let row = sqlx::query(&format!("SELECT {COLS} FROM staff WHERE lower(email) = lower(?)"))
        .bind(email.trim())
        .fetch_optional(pool)
        .await
        .db_err()?;
    Ok(row.as_ref().map(map))
}

/// Authenticate an operator. Constant-cost against an unknown address, for
/// the same reason the customer path is.
pub async fn authenticate(pool: &Db, email: &str, password: &str) -> Result<Staff> {
    const DUMMY: &str = "$2b$12$C6UzMDM.H6dfI/f/IKcEeO3Wz1W4J1mP6RhJ0BCGcmJjK0KjCIWxK";
    let found = find_by_email(pool, email).await?;
    let hash = found.as_ref().map(|s| s.password_hash.as_str()).unwrap_or(DUMMY);
    let ok = verify_password(password, hash);

    match found {
        Some(staff) if ok => {
            if !staff.is_active() {
                return Err(ProError::Forbidden("This login has been disabled.".into()));
            }
            sqlx::query("UPDATE staff SET last_login_at = ? WHERE id = ?")
                .bind(now_iso())
                .bind(&staff.id)
                .execute(pool)
                .await
                .db_err()?;
            Ok(staff)
        }
        _ => Err(ProError::Unauthorized("Email or password is incorrect.".into())),
    }
}

pub async fn list(pool: &Db) -> Result<Vec<Staff>> {
    let rows = sqlx::query(&format!("SELECT {COLS} FROM staff ORDER BY created_at"))
        .fetch_all(pool)
        .await
        .db_err()?;
    Ok(rows.iter().map(map).collect())
}

pub async fn count(pool: &Db) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM staff").fetch_one(pool).await.db_err()
}

async fn count_active_owners(pool: &Db) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM staff WHERE role = 'owner' AND status = 'active'")
        .fetch_one(pool)
        .await
        .db_err()
}

/// Change a staff member's role.
///
/// `actor` is the person making the change, and two rules are enforced here
/// rather than in the UI:
///
/// * nobody may grant a role above their own — otherwise an administrator
///   promotes themselves to owner and reaches the payment settings;
/// * the last active owner may not be demoted, because a platform with no
///   owner has no one who can change the payment provider, and no way to
///   appoint one.
pub async fn set_role(
    pool: &Db,
    actor: &Staff,
    target_id: &str,
    role: StaffRole,
) -> Result<()> {
    if !actor.assignable_contains(role) {
        return Err(ProError::Forbidden(format!(
            "You may not grant the {} role.",
            role.label()
        )));
    }
    let target = get(pool, target_id).await?;
    if target.role == StaffRole::Owner
        && role != StaffRole::Owner
        && count_active_owners(pool).await? <= 1
    {
        return Err(ProError::Conflict(
            "This is the only owner. Promote somebody else to owner first.".into(),
        ));
    }
    sqlx::query("UPDATE staff SET role = ? WHERE id = ?")
        .bind(role.as_str())
        .bind(target_id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Disable or re-enable a login.
///
/// Disabling also ends that person's sessions — otherwise they stay signed in
/// until their cookie expires, which is not what "disabled" means to whoever
/// clicked it.
pub async fn set_status(
    pool: &Db,
    actor: &Staff,
    target_id: &str,
    status: &str,
) -> Result<()> {
    if actor.role < StaffRole::Owner {
        return Err(ProError::Forbidden("Only an owner may enable or disable a staff login.".into()));
    }
    if actor.id == target_id && status != "active" {
        // Not a safety rail so much as a courtesy: locking yourself out of
        // the console is recoverable only from the command line.
        return Err(ProError::BadRequest("You cannot disable your own login.".into()));
    }
    let target = get(pool, target_id).await?;
    if target.role == StaffRole::Owner && status != "active" && count_active_owners(pool).await? <= 1
    {
        return Err(ProError::Conflict(
            "This is the only owner. Promote somebody else to owner first.".into(),
        ));
    }
    sqlx::query("UPDATE staff SET status = ? WHERE id = ?")
        .bind(status)
        .bind(target_id)
        .execute(pool)
        .await
        .db_err()?;
    if status != "active" {
        crate::sessions::destroy_all(pool, crate::sessions::Subject::Staff, target_id).await?;
    }
    Ok(())
}

/// Change a password, ending every other session for that person.
pub async fn set_password(pool: &Db, id: &str, password: &str) -> Result<()> {
    if password.len() < 12 {
        return Err(ProError::BadRequest(
            "A staff password must be at least 12 characters.".into(),
        ));
    }
    sqlx::query("UPDATE staff SET password_hash = ? WHERE id = ?")
        .bind(hash_password(password)?)
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    crate::sessions::destroy_all(pool, crate::sessions::Subject::Staff, id).await?;
    Ok(())
}

pub async fn rename(pool: &Db, id: &str, name: &str) -> Result<()> {
    sqlx::query("UPDATE staff SET name = ? WHERE id = ?")
        .bind(name.trim())
        .bind(id)
        .execute(pool)
        .await
        .db_err()?;
    Ok(())
}

/// Create the first owner, if there is no staff at all.
///
/// This is what `ghost-pro setup` calls. It is a no-op once anybody exists, so
/// it cannot be used to mint a second owner on a running platform.
pub async fn bootstrap_owner(
    pool: &Db,
    email: &str,
    name: &str,
    password: &str,
) -> Result<Option<Staff>> {
    if count(pool).await? > 0 {
        return Ok(None);
    }
    create(pool, email, name, password, StaffRole::Owner).await.map(Some)
}

/// Extension used by [`set_role`], kept as a trait method so the rule lives
/// next to the role type rather than being re-derived at each call site.
trait Assignable {
    fn assignable_contains(&self, role: StaffRole) -> bool;
}

impl Assignable for Staff {
    fn assignable_contains(&self, role: StaffRole) -> bool {
        self.is_active() && self.role.assignable().contains(&role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Db, Staff) {
        let pool = crate::pool::open_in_memory().await.unwrap();
        let owner = bootstrap_owner(&pool, "owner@example.com", "Owner", "a-long-password")
            .await
            .unwrap()
            .unwrap();
        (pool, owner)
    }

    #[tokio::test]
    async fn bootstraps_exactly_one_owner() {
        let (pool, owner) = fixture().await;
        assert_eq!(owner.role, StaffRole::Owner);
        // A second call is a no-op, so this cannot mint an owner later.
        assert!(bootstrap_owner(&pool, "other@example.com", "O", "a-long-password")
            .await
            .unwrap()
            .is_none());
        assert_eq!(count(&pool).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn authenticates() {
        let (pool, owner) = fixture().await;
        let s = authenticate(&pool, "owner@example.com", "a-long-password").await.unwrap();
        assert_eq!(s.id, owner.id);
        assert!(get(&pool, &owner.id).await.unwrap().last_login_at.is_some());
        assert_eq!(
            authenticate(&pool, "owner@example.com", "wrong").await.unwrap_err().status(),
            401
        );
    }

    /// Unknown address and wrong password must be indistinguishable.
    #[tokio::test]
    async fn does_not_reveal_whether_a_login_exists() {
        let (pool, _) = fixture().await;
        let a = authenticate(&pool, "owner@example.com", "nope").await.unwrap_err();
        let b = authenticate(&pool, "nobody@example.com", "nope").await.unwrap_err();
        assert_eq!(a.to_string(), b.to_string());
    }

    /// Staff passwords have a higher floor than customers' — these logins can
    /// suspend every site or redirect revenue.
    #[tokio::test]
    async fn requires_a_long_password() {
        let (pool, _) = fixture().await;
        assert!(create(&pool, "s@example.com", "S", "short", StaffRole::Support).await.is_err());
        assert!(set_password(&pool, "whoever", "short").await.is_err());
    }

    /// The rule that keeps the payment settings out of reach: an admin cannot
    /// promote anybody — including themselves — to owner.
    #[tokio::test]
    async fn an_admin_cannot_create_an_owner() {
        let (pool, owner) = fixture().await;
        let admin = create(&pool, "admin@example.com", "Admin", "a-long-password", StaffRole::Admin)
            .await
            .unwrap();

        let err = set_role(&pool, &admin, &admin.id, StaffRole::Owner).await.unwrap_err();
        assert_eq!(err.status(), 403);
        assert_eq!(get(&pool, &admin.id).await.unwrap().role, StaffRole::Admin);

        // An owner can.
        set_role(&pool, &owner, &admin.id, StaffRole::Owner).await.unwrap();
        assert_eq!(get(&pool, &admin.id).await.unwrap().role, StaffRole::Owner);
    }

    #[tokio::test]
    async fn an_admin_may_still_appoint_support() {
        let (pool, owner) = fixture().await;
        let admin = create(&pool, "admin@example.com", "Admin", "a-long-password", StaffRole::Admin)
            .await
            .unwrap();
        let helper = create(&pool, "help@example.com", "H", "a-long-password", StaffRole::Support)
            .await
            .unwrap();
        set_role(&pool, &admin, &helper.id, StaffRole::Support).await.unwrap();
        // And an owner outranks everything.
        set_role(&pool, &owner, &helper.id, StaffRole::Admin).await.unwrap();
        assert_eq!(get(&pool, &helper.id).await.unwrap().role, StaffRole::Admin);
    }

    /// A platform with no owner has nobody who can change the payment
    /// provider and no way to appoint one.
    #[tokio::test]
    async fn the_last_owner_cannot_be_demoted() {
        let (pool, owner) = fixture().await;
        let err = set_role(&pool, &owner, &owner.id, StaffRole::Admin).await.unwrap_err();
        assert_eq!(err.status(), 409);
        assert_eq!(get(&pool, &owner.id).await.unwrap().role, StaffRole::Owner);

        // With a second owner in place, the first may step down.
        let second = create(&pool, "two@example.com", "Two", "a-long-password", StaffRole::Owner)
            .await
            .unwrap();
        set_role(&pool, &owner, &owner.id, StaffRole::Admin).await.unwrap();
        assert_eq!(get(&pool, &owner.id).await.unwrap().role, StaffRole::Admin);
        assert_eq!(get(&pool, &second.id).await.unwrap().role, StaffRole::Owner);
    }

    #[tokio::test]
    async fn the_last_owner_cannot_be_disabled() {
        let (pool, owner) = fixture().await;
        let second = create(&pool, "two@example.com", "Two", "a-long-password", StaffRole::Owner)
            .await
            .unwrap();
        // Self-disable is refused outright.
        assert!(set_status(&pool, &owner, &owner.id, "disabled").await.is_err());
        // Disabling the other owner is fine while one remains.
        set_status(&pool, &owner, &second.id, "disabled").await.unwrap();
        assert!(!get(&pool, &second.id).await.unwrap().is_active());
    }

    /// Disabling must end the sessions too, or "disabled" does not mean
    /// signed out.
    #[tokio::test]
    async fn disabling_ends_their_sessions() {
        let (pool, owner) = fixture().await;
        let helper = create(&pool, "h@example.com", "H", "a-long-password", StaffRole::Support)
            .await
            .unwrap();
        let token =
            crate::sessions::create(&pool, crate::sessions::Subject::Staff, &helper.id, None, None)
                .await
                .unwrap();
        assert!(crate::sessions::resolve(&pool, crate::sessions::Subject::Staff, &token).await.is_ok());

        set_status(&pool, &owner, &helper.id, "disabled").await.unwrap();
        assert!(crate::sessions::resolve(&pool, crate::sessions::Subject::Staff, &token)
            .await
            .is_err());
        assert_eq!(
            authenticate(&pool, "h@example.com", "a-long-password").await.unwrap_err().status(),
            403
        );
    }

    /// A password change signs every device out.
    #[tokio::test]
    async fn changing_a_password_ends_sessions() {
        let (pool, owner) = fixture().await;
        let token =
            crate::sessions::create(&pool, crate::sessions::Subject::Staff, &owner.id, None, None)
                .await
                .unwrap();
        set_password(&pool, &owner.id, "another-long-password").await.unwrap();
        assert!(crate::sessions::resolve(&pool, crate::sessions::Subject::Staff, &token)
            .await
            .is_err());
        assert!(authenticate(&pool, "owner@example.com", "another-long-password").await.is_ok());
    }

    /// Only an owner may enable or disable a login at all.
    #[tokio::test]
    async fn an_admin_cannot_disable_logins() {
        let (pool, _) = fixture().await;
        let admin = create(&pool, "a@example.com", "A", "a-long-password", StaffRole::Admin)
            .await
            .unwrap();
        let helper = create(&pool, "h@example.com", "H", "a-long-password", StaffRole::Support)
            .await
            .unwrap();
        assert_eq!(
            set_status(&pool, &admin, &helper.id, "disabled").await.unwrap_err().status(),
            403
        );
    }

    #[tokio::test]
    async fn refuses_a_duplicate_email() {
        let (pool, _) = fixture().await;
        let err = create(&pool, "OWNER@example.com", "Dup", "a-long-password", StaffRole::Admin)
            .await
            .unwrap_err();
        assert_eq!(err.status(), 409);
    }

    /// A corrupt or future role reads as the least privileged, never the most.
    #[tokio::test]
    async fn an_unknown_role_reads_as_support() {
        let (pool, owner) = fixture().await;
        sqlx::query("UPDATE staff SET role = 'archduke' WHERE id = ?")
            .bind(&owner.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(get(&pool, &owner.id).await.unwrap().role, StaffRole::Support);
    }

    #[tokio::test]
    async fn lists_staff() {
        let (pool, _) = fixture().await;
        create(&pool, "b@example.com", "B", "a-long-password", StaffRole::Support).await.unwrap();
        assert_eq!(list(&pool).await.unwrap().len(), 2);
    }
}
