pub mod info;
pub mod list;

pub(super) fn full_node_name(namespace: &str, name: &str) -> String {
    if namespace == "/" {
        format!("/{name}")
    } else {
        format!("{namespace}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::full_node_name;

    #[test]
    fn joins_root_namespace_and_node_name() {
        assert_eq!(full_node_name("/", "talker"), "/talker");
    }

    #[test]
    fn joins_nested_namespace_and_node_name() {
        assert_eq!(full_node_name("/ns", "talker"), "/ns/talker");
    }
}
