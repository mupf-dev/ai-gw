// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Rollen und Berechtigungen der Admin-API (§15, AIGW-32).
//!
//! Eine Rollenzuweisung ([`RoleBinding`]) gilt global oder eingeschränkt auf eine
//! Organisation bzw. ein Team. Jede Operation fordert eine [`Permission`] auf einem
//! [`Target`] an; erlaubt ist sie, wenn **eine** Zuweisung sie abdeckt:
//!
//! | Rolle | Lesen | Schreiben |
//! |---|---|---|
//! | `admin` | alles im Geltungsbereich | alles im Geltungsbereich |
//! | `org-admin` (Org O) | O, Teams und Keys in O, Katalog | Teams und Keys in O (nicht O selbst) |
//! | `team-admin` (Team T) | T, Keys in T, die Org von T (falls angegeben), Katalog | Keys in T |
//! | `viewer` | alles im Geltungsbereich, Katalog | – |
//! | `auditor` | wie `viewer` (Audit-Log folgt) | – |
//!
//! `org-admin` ohne Organisation und `team-admin` ohne Team verleihen keine Rechte.
//! Globale Ziele (Nutzer, Admin-API-Keys, Anlegen von Organisationen) erfordern eine
//! globale Zuweisung; schreiben darf dort nur `admin`.

use aigw_core::config::GroupRoleMapping;
use aigw_core::resource::{Role, RoleBinding};

/// Benötigte Berechtigung einer Operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Lesen.
    Read,
    /// Ändern.
    Write,
    /// Gespeicherte Request-/Response-Inhalte (Payload-Logging) lesen: nur `admin` und
    /// `auditor` im jeweiligen Geltungsbereich (AIGW-46).
    ReadPayload,
}

/// Ziel einer Operation (Geltungsbereich).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Globale Objekte: Nutzer, Admin-API-Keys, Anlegen von Organisationen, Einstellungen.
    Global,
    /// Gemeinsamer Katalog (Modellgruppen): lesen darf jeder mit einer wirksamen Rolle,
    /// schreiben nur ein globaler `admin`.
    Catalog,
    /// Eine Organisation selbst.
    Organization(String),
    /// Ein Team selbst.
    Team {
        /// Organisation des Teams.
        organization: String,
        /// Team.
        team: String,
    },
    /// Etwas innerhalb eines Teams (API-Keys, deren Limits und Budgets).
    InTeam {
        /// Organisation des Teams.
        organization: String,
        /// Team.
        team: String,
    },
}

impl Target {
    /// Team-Objekt.
    pub fn team(organization: impl Into<String>, team: impl Into<String>) -> Self {
        Self::Team {
            organization: organization.into(),
            team: team.into(),
        }
    }

    /// Objekt in einem Team.
    pub fn in_team(organization: impl Into<String>, team: impl Into<String>) -> Self {
        Self::InTeam {
            organization: organization.into(),
            team: team.into(),
        }
    }

    fn organization(&self) -> Option<&str> {
        match self {
            Self::Organization(o)
            | Self::Team {
                organization: o, ..
            }
            | Self::InTeam {
                organization: o, ..
            } => Some(o),
            Self::Global | Self::Catalog => None,
        }
    }

    fn team_name(&self) -> Option<&str> {
        match self {
            Self::Team { team, .. } | Self::InTeam { team, .. } => Some(team),
            _ => None,
        }
    }
}

/// Geltungsbereich einer Zuweisung.
enum Scope<'a> {
    Global,
    Organization(&'a str),
    Team(Option<&'a str>, &'a str),
}

fn scope(b: &RoleBinding) -> Scope<'_> {
    match (b.organization.as_deref(), b.team.as_deref()) {
        (o, Some(t)) => Scope::Team(o, t),
        (Some(o), None) => Scope::Organization(o),
        (None, None) => Scope::Global,
    }
}

/// `true`, wenn die Zuweisung eine wirksame Rolle ist (eingeschränkte Rollen brauchen ihren
/// Geltungsbereich).
pub fn is_effective(b: &RoleBinding) -> bool {
    match b.role {
        Role::Admin | Role::Viewer | Role::Auditor => true,
        Role::OrgAdmin => b.organization.is_some() || b.team.is_some(),
        Role::TeamAdmin => b.team.is_some(),
    }
}

/// Prüft eine einzelne Zuweisung.
pub fn binding_allows(b: &RoleBinding, perm: Permission, target: &Target) -> bool {
    if !is_effective(b) {
        return false;
    }
    // Katalog: lesen darf jede wirksame Rolle, schreiben nur ein globaler Admin.
    if *target == Target::Catalog {
        return match perm {
            Permission::Read => true,
            Permission::Write => b.role == Role::Admin && matches!(scope(b), Scope::Global),
            Permission::ReadPayload => false,
        };
    }
    let covers = match scope(b) {
        Scope::Global => true,
        Scope::Organization(o) => target.organization() == Some(o),
        Scope::Team(o, t) => match target {
            Target::Team { .. } | Target::InTeam { .. } => {
                target.team_name() == Some(t) && o.is_none_or(|o| target.organization() == Some(o))
            }
            // Die Organisation des eigenen Teams lesen (nur wenn die Zuweisung sie nennt).
            Target::Organization(org) => perm != Permission::Write && o == Some(org.as_str()),
            Target::Global | Target::Catalog => false,
        },
    };
    if !covers {
        return false;
    }
    match (b.role, perm) {
        (_, Permission::Read)
        | (Role::Admin, Permission::Write)
        | (Role::Admin | Role::Auditor, Permission::ReadPayload) => true,
        (_, Permission::ReadPayload) => false,
        (Role::OrgAdmin, Permission::Write) => {
            matches!(target, Target::Team { .. } | Target::InTeam { .. })
        }
        (Role::TeamAdmin, Permission::Write) => matches!(target, Target::InTeam { .. }),
        (Role::Viewer | Role::Auditor, Permission::Write) => false,
    }
}

/// Prüft eine Menge von Zuweisungen.
pub fn allows(bindings: &[RoleBinding], perm: Permission, target: &Target) -> bool {
    bindings.iter().any(|b| binding_allows(b, perm, target))
}

/// `true`, wenn eine Zuweisung globaler `admin` ist.
pub fn is_global_admin(bindings: &[RoleBinding]) -> bool {
    bindings
        .iter()
        .any(|b| b.role == Role::Admin && b.organization.is_none() && b.team.is_none())
}

/// Rollen aus IdP-Gruppen laut Konfiguration.
pub fn roles_from_groups(groups: &[String], mappings: &[GroupRoleMapping]) -> Vec<RoleBinding> {
    let mut out: Vec<RoleBinding> = Vec::new();
    for m in mappings {
        if groups.contains(&m.group) {
            let b = RoleBinding {
                role: m.role,
                organization: m.organization.clone(),
                team: m.team.clone(),
            };
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out
}

/// Vereinigung zweier Rollenlisten ohne Dubletten.
pub fn merge(mut a: Vec<RoleBinding>, b: Vec<RoleBinding>) -> Vec<RoleBinding> {
    for x in b {
        if !a.contains(&x) {
            a.push(x);
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rb(role: Role, org: Option<&str>, team: Option<&str>) -> RoleBinding {
        RoleBinding {
            role,
            organization: org.map(Into::into),
            team: team.map(Into::into),
        }
    }

    const R: Permission = Permission::Read;
    const W: Permission = Permission::Write;

    #[test]
    fn admin_global_and_scoped() {
        let g = [rb(Role::Admin, None, None)];
        for t in [
            Target::Global,
            Target::Catalog,
            Target::Organization("x".into()),
            Target::in_team("x", "t"),
        ] {
            assert!(allows(&g, W, &t));
        }
        let scoped = [rb(Role::Admin, Some("acme"), None)];
        assert!(allows(&scoped, W, &Target::Organization("acme".into())));
        assert!(!allows(&scoped, R, &Target::Organization("other".into())));
        assert!(!allows(&scoped, R, &Target::Global));
        assert!(!allows(&scoped, W, &Target::Catalog));
        assert!(allows(&scoped, R, &Target::Catalog));
        assert!(is_global_admin(&g) && !is_global_admin(&scoped));
    }

    #[test]
    fn org_admin() {
        let b = [rb(Role::OrgAdmin, Some("acme"), None)];
        assert!(allows(&b, R, &Target::Organization("acme".into())));
        assert!(!allows(&b, W, &Target::Organization("acme".into())));
        assert!(allows(&b, W, &Target::team("acme", "t1")));
        assert!(allows(&b, W, &Target::in_team("acme", "t1")));
        assert!(!allows(&b, R, &Target::team("other", "t2")));
        assert!(!allows(&b, R, &Target::Global));
        // ohne Organisation wirkungslos
        assert!(!allows(
            &[rb(Role::OrgAdmin, None, None)],
            R,
            &Target::Catalog
        ));
    }

    #[test]
    fn team_admin() {
        let b = [rb(Role::TeamAdmin, Some("acme"), Some("platform"))];
        assert!(allows(&b, W, &Target::in_team("acme", "platform")));
        assert!(allows(&b, R, &Target::team("acme", "platform")));
        assert!(!allows(&b, W, &Target::team("acme", "platform")));
        assert!(!allows(&b, W, &Target::in_team("acme", "other")));
        assert!(!allows(&b, R, &Target::in_team("acme", "other")));
        // gleicher Teamname, andere Organisation (Zuweisung nennt die Organisation)
        assert!(!allows(&b, W, &Target::in_team("evil", "platform")));
        assert!(allows(&b, R, &Target::Organization("acme".into())));
        assert!(!allows(&b, W, &Target::Organization("acme".into())));
        assert!(allows(&b, R, &Target::Catalog));
        assert!(!allows(&b, R, &Target::Global));
        // ohne Team wirkungslos
        assert!(!allows(
            &[rb(Role::TeamAdmin, Some("acme"), None)],
            R,
            &Target::Organization("acme".into())
        ));
    }

    #[test]
    fn viewer_and_auditor_read_only() {
        for role in [Role::Viewer, Role::Auditor] {
            let g = [rb(role, None, None)];
            assert!(allows(&g, R, &Target::Global));
            assert!(!allows(&g, W, &Target::in_team("a", "b")));
            let t = [rb(role, None, Some("t"))];
            assert!(allows(&t, R, &Target::in_team("a", "t")));
            assert!(!allows(&t, R, &Target::in_team("a", "u")));
        }
    }

    #[test]
    fn payload_only_admin_and_auditor() {
        const P: Permission = Permission::ReadPayload;
        let t = Target::in_team("acme", "platform");
        for (role, ok) in [
            (Role::Admin, true),
            (Role::Auditor, true),
            (Role::Viewer, false),
            (Role::OrgAdmin, false),
            (Role::TeamAdmin, false),
        ] {
            let b = [rb(role, Some("acme"), Some("platform"))];
            assert_eq!(allows(&b, P, &t), ok, "{role:?}");
            assert!(allows(&b, R, &t), "{role:?}");
        }
        // Geltungsbereich gilt auch hier
        let a = [rb(Role::Auditor, Some("acme"), None)];
        assert!(allows(&a, P, &t));
        assert!(!allows(&a, P, &Target::in_team("other", "x")));
        assert!(!allows(&a, P, &Target::Global));
        assert!(allows(&[rb(Role::Auditor, None, None)], P, &Target::Global));
        assert!(!allows(&[rb(Role::Admin, None, None)], P, &Target::Catalog));
    }

    #[test]
    fn group_mapping() {
        let m = vec![
            GroupRoleMapping {
                group: "admins".into(),
                role: Role::Admin,
                organization: None,
                team: None,
            },
            GroupRoleMapping {
                group: "leads".into(),
                role: Role::TeamAdmin,
                organization: Some("acme".into()),
                team: Some("platform".into()),
            },
        ];
        assert!(roles_from_groups(&["x".into()], &m).is_empty());
        let r = roles_from_groups(&["leads".into(), "admins".into()], &m);
        assert_eq!(r.len(), 2);
        let merged = merge(r.clone(), r);
        assert_eq!(merged.len(), 2);
    }
}
