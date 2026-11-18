//! Helpers for unit tests.

use corax_config::LoadedConfig;

use crate::World;

pub const OBJ: &str = "define command {\n command_name ping\n command_line $USER1$/check_icmp $ARG1$ $HOSTADDRESS$\n}\n\
    define command {\n command_name notify\n command_line NOTIFY_WHAT='$NOTIFICATIONTYPE$' /bin/true\n}\n\
    define contact {\n contact_name admin\n host_notification_commands notify\n service_notification_commands notify\n}\n\
    define contactgroup {\n contactgroup_name admins\n members admin\n}\n\
    define host {\n host_name h1\n address 10.1.1.1\n max_check_attempts 1\n check_command ping!-w $HOSTNAME$\n contact_groups admins\n _TAGS a b\n}\n\
    define service {\n host_name h1\n service_description CPU load\n check_command ping\n max_check_attempts 3\n}\n";

/// Build a world from object definitions (with `$USER1$=/usr/lib/plugins`).
pub fn world_from(objects: &str) -> World {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.cfg");
    std::fs::write(&path, objects).unwrap();
    let (objs, _) = corax_config::load_objects(&[path]).unwrap();
    let mut resources = corax_config::Resources::default();
    resources.parse_into("$USER1$=/usr/lib/plugins\n");
    let cfg = LoadedConfig { main: Default::default(), resources, objects: objs, warnings: vec![] };
    World::new(&cfg, 1_700_000_000)
}
