//! The guest's users, from /etc/passwd and /etc/group (we are static, so the C library
//! cannot ask the distribution's NSS modules).

use super::sys::Error;

/// A user that sessions run as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
    /// The supplementary groups (including the primary one).
    pub groups: Vec<u32>,
}

/// Looks up a user by name.
pub fn lookup(name: &str) -> Result<User, Error> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    match parse(&passwd, &group, name) {
        Some(user) => Ok(user),
        // root works even if the distribution has no passwd yet.
        None if name == "root" => Ok(User {
            name: "root".into(),
            uid: 0,
            gid: 0,
            home: "/root".into(),
            shell: "/bin/sh".into(),
            groups: vec![0],
        }),
        None => Err(Error::Message(format!("no user {name} in the guest"))),
    }
}

/// The ID of a group.
pub fn group_id(name: &str) -> Option<u32> {
    let group = std::fs::read_to_string("/etc/group").ok()?;
    group
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.len() >= 3 && fields[0] == name)
        .and_then(|fields| fields[2].parse().ok())
}

/// The login shell of the user with `uid` (/bin/sh if we do not know it).
pub fn shell_of(uid: u32) -> String {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let name = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.len() >= 7 && fields[2].parse() == Ok(uid))
        .map(|fields| fields[0].to_string());
    name.and_then(|name| parse(&passwd, "", &name))
        .map_or_else(|| "/bin/sh".into(), |user| user.shell)
}

fn parse(passwd: &str, group: &str, name: &str) -> Option<User> {
    let fields: Vec<&str> = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.len() >= 7 && fields[0] == name)?;
    let uid = fields[2].parse().ok()?;
    let gid = fields[3].parse().ok()?;
    let mut groups = vec![gid];
    for fields in group
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
    {
        if fields.len() >= 4
            && fields[3].split(',').any(|member| member == name)
            && let Ok(id) = fields[2].parse()
            && !groups.contains(&id)
        {
            groups.push(id);
        }
    }
    let shell = Some(fields[6]).filter(|shell| !shell.is_empty());
    Some(User {
        name: name.to_string(),
        uid,
        gid,
        home: fields[5].to_string(),
        shell: shell.unwrap_or("/bin/sh").to_string(),
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_users_and_groups() {
        let passwd = "root:x:0:0:root:/root:/bin/ash\n\
                      alice:x:1000:1000:Alice:/home/alice:\n";
        let group = "root:x:0:root\nwheel:x:10:root,alice\nvideo:x:27:bob,alice\n";
        let alice = parse(passwd, group, "alice").unwrap();
        assert_eq!((alice.uid, alice.gid), (1000, 1000));
        assert_eq!(alice.home, "/home/alice");
        assert_eq!(alice.shell, "/bin/sh");
        assert_eq!(alice.groups, [1000, 10, 27]);
        let root = parse(passwd, group, "root").unwrap();
        assert_eq!(
            (root.shell.as_str(), root.groups.as_slice()),
            ("/bin/ash", &[0, 10][..])
        );
        assert_eq!(parse(passwd, group, "bob"), None);
    }
}
