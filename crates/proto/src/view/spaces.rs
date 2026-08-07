//! Spaces-section derivations (gh#124): the honest space row and the
//! device-once grouping.
//!
//! A space row used to spend its loudest pixels on its least differentiating
//! fact — every row repeating "@ box · offline" in warning amber. The rules
//! here fix that at the derivation layer so both viewports agree:
//!
//! - [`space_title`] — what a space row is CALLED. A space is repo-first
//!   (gh#118): the `owner/repo` slug names it whenever a host has supplied the
//!   link, an explicit rename still wins, and the folder basename is the
//!   fallback for the scratch directory that is nobody's repo.
//! - [`device_groups`] — where the device's name goes: once, as a group
//!   header above the spaces it hosts, instead of riding along on every row.
//!   The local device leads; remaining groups keep the spaces' given order.

use crate::Space;

/// One device's spaces, in the caller's (manual/creation) order.
///
/// The grouping preserves the input order within a group, so a caller that has
/// already applied a manual drag order keeps it. Reordering across groups is
/// meaningless — a space cannot change its device by being dragged.
#[derive(Debug, PartialEq)]
pub struct DeviceGroup<'a> {
    pub device_id: &'a str,
    pub spaces: Vec<&'a Space>,
}

/// Group ordered spaces by owning device: the local device's group first (your
/// own folders before any box's), then groups in first-appearance order of the
/// input. Pure and total — every space lands in exactly one group.
pub fn device_groups<'a>(ordered: &'a [Space], local_device: Option<&str>) -> Vec<DeviceGroup<'a>> {
    let mut groups: Vec<DeviceGroup<'a>> = Vec::new();
    for space in ordered {
        match groups.iter_mut().find(|g| g.device_id == space.device_id) {
            Some(group) => group.spaces.push(space),
            None => groups.push(DeviceGroup {
                device_id: &space.device_id,
                spaces: vec![space],
            }),
        }
    }
    if let Some(local) = local_device
        && let Some(ix) = groups.iter().position(|g| g.device_id == local)
        && ix > 0
    {
        let group = groups.remove(ix);
        groups.insert(0, group);
    }
    groups
}

/// What a space row is called: an explicit rename wins, then the repo slug
/// (`owner/repo`, per the gh#118 links), then the folder basename. Pure.
pub fn space_title<'a>(space: &'a Space, slug: Option<&'a str>) -> &'a str {
    if let Some(name) = space.name.as_deref()
        && !name.trim().is_empty()
    {
        return name;
    }
    if let Some(slug) = slug
        && !slug.trim().is_empty()
    {
        return slug;
    }
    space.display_name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn space(id: &str, device: &str, path: &str) -> Space {
        Space {
            id: id.into(),
            device_id: device.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn groups_preserve_input_order_within_a_device() {
        let spaces = vec![
            space("s1", "box", "/srv/a"),
            space("s2", "box", "/srv/b"),
            space("s3", "box", "/srv/c"),
        ];
        let groups = device_groups(&spaces, None);
        assert_eq!(groups.len(), 1);
        let ids: Vec<&str> = groups[0].spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2", "s3"]);
    }

    #[test]
    fn local_device_group_leads() {
        let spaces = vec![
            space("s1", "box", "/srv/a"),
            space("s2", "laptop", "/home/a"),
            space("s3", "box", "/srv/b"),
        ];
        let groups = device_groups(&spaces, Some("laptop"));
        assert_eq!(groups[0].device_id, "laptop");
        assert_eq!(groups[1].device_id, "box");
        // The box group keeps the input's relative order.
        let ids: Vec<&str> = groups[1].spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s3"]);
    }

    #[test]
    fn unknown_local_device_keeps_first_appearance_order() {
        let spaces = vec![
            space("s1", "box", "/srv/a"),
            space("s2", "laptop", "/home/a"),
        ];
        let groups = device_groups(&spaces, Some("phone"));
        assert_eq!(groups[0].device_id, "box");
        assert_eq!(groups[1].device_id, "laptop");
    }

    #[test]
    fn no_spaces_no_groups() {
        assert!(device_groups(&[], Some("laptop")).is_empty());
    }

    #[test]
    fn title_prefers_rename_then_slug_then_basename() {
        let mut s = space("s1", "box", "/srv/comet-board");
        assert_eq!(space_title(&s, None), "comet-board");
        assert_eq!(space_title(&s, Some("brede/comet-board")), "brede/comet-board");
        s.name = Some("My board".into());
        assert_eq!(space_title(&s, Some("brede/comet-board")), "My board");
    }

    #[test]
    fn blank_slug_and_blank_rename_fall_through() {
        let mut s = space("s1", "box", "/srv/comet-board");
        s.name = Some("  ".into());
        assert_eq!(space_title(&s, Some("  ")), "comet-board");
    }
}
