use super::{SavedRequest, current_timestamp};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeItem {
    Request(String),
    Folder(String),
}

pub fn folder_parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

pub fn folder_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn in_folder(path: &str, folder: &str) -> bool {
    path == folder || path.starts_with(&format!("{folder}/"))
}

pub fn folder_ancestors(path: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut current = String::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        ancestors.push(current.clone());
    }
    ancestors
}

pub fn validate_folder_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty()
        || name != name.trim()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\'])
    {
        return Err("文件夹名称不能为空，不能为“.”或“..”，也不能包含路径分隔符。".into());
    }
    Ok(())
}

pub fn remap_folder(path: &str, source: &str, destination: &str) -> String {
    if in_folder(path, source) {
        format!("{destination}{}", &path[source.len()..])
    } else {
        path.to_owned()
    }
}

fn validate_parent(folders: &[String], parent: &str) -> Result<(), String> {
    if !parent.is_empty() && !folders.iter().any(|folder| folder == parent) {
        return Err("目标文件夹不存在。".into());
    }
    Ok(())
}

pub fn relocate_folder(
    requests: &mut [SavedRequest],
    folders: &mut [String],
    source: &str,
    destination: &str,
) -> Result<(), String> {
    if !folders.iter().any(|folder| folder == source) {
        return Err("源文件夹不存在。".into());
    }
    validate_folder_name(folder_name(destination))?;
    validate_parent(folders, folder_parent(destination))?;
    if source == destination {
        return Ok(());
    }
    if in_folder(folder_parent(destination), source) {
        return Err("不能将文件夹移动到自身或它的子文件夹中。".into());
    }
    for folder in folders.iter().filter(|folder| in_folder(folder, source)) {
        let replacement = remap_folder(folder, source, destination);
        if folders.iter().any(|existing| {
            !in_folder(existing, source) && existing.eq_ignore_ascii_case(&replacement)
        }) {
            return Err(format!("目标位置已经存在文件夹“{replacement}”。"));
        }
    }
    for folder in folders.iter_mut() {
        *folder = remap_folder(folder, source, destination);
    }
    for request in requests {
        if in_folder(&request.folder, source) {
            request.folder = remap_folder(&request.folder, source, destination);
            request.updated_at = current_timestamp();
        }
    }
    Ok(())
}

pub fn move_item(
    requests: &mut Vec<SavedRequest>,
    folders: &mut Vec<String>,
    item: &TreeItem,
    parent: &str,
    before: Option<&TreeItem>,
) -> Result<TreeItem, String> {
    validate_parent(folders, parent)?;
    match item {
        TreeItem::Request(id) => {
            let index = requests
                .iter()
                .position(|request| request.id == *id)
                .ok_or_else(|| "请求不存在。".to_string())?;
            let before_id = match before {
                Some(TreeItem::Request(target)) => {
                    let target_request = requests
                        .iter()
                        .find(|request| request.id == *target)
                        .ok_or_else(|| "排序目标请求不存在。".to_string())?;
                    if target_request.folder != parent {
                        return Err("排序目标不在目标文件夹中。".into());
                    }
                    if id == target {
                        return Ok(item.clone());
                    }
                    Some(target)
                }
                Some(TreeItem::Folder(_)) => None,
                None => None,
            };
            let mut request = requests.remove(index);
            request.folder = parent.to_owned();
            request.updated_at = current_timestamp();
            let insertion = if let Some(target) = before_id {
                requests
                    .iter()
                    .position(|request| request.id == *target)
                    .unwrap()
            } else {
                requests
                    .iter()
                    .rposition(|request| request.folder == parent)
                    .map_or(requests.len(), |index| index + 1)
            };
            requests.insert(insertion, request);
            Ok(item.clone())
        }
        TreeItem::Folder(source) => {
            if in_folder(parent, source) {
                return Err("不能将文件夹移动到自身或它的子文件夹中。".into());
            }
            let before_folder = match before {
                Some(TreeItem::Folder(target)) => {
                    if !folders.contains(target) || folder_parent(target) != parent {
                        return Err("排序目标文件夹不存在或不在目标层级。".into());
                    }
                    if target == source {
                        return Ok(item.clone());
                    }
                    Some(target)
                }
                _ => None,
            };
            let destination = if parent.is_empty() {
                folder_name(source).to_owned()
            } else {
                format!("{parent}/{}", folder_name(source))
            };
            relocate_folder(requests, folders, source, &destination)?;
            let mut subtree = Vec::new();
            folders.retain(|folder| {
                if in_folder(folder, &destination) {
                    subtree.push(folder.clone());
                    false
                } else {
                    true
                }
            });
            let insertion = if let Some(target) = before_folder {
                folders.iter().position(|folder| folder == target).unwrap()
            } else if parent.is_empty() {
                folders.len()
            } else {
                folders
                    .iter()
                    .rposition(|folder| in_folder(folder, parent))
                    .map_or(folders.len(), |index| index + 1)
            };
            folders.splice(insertion..insertion, subtree);
            Ok(TreeItem::Folder(destination))
        }
    }
}

pub fn delete_folder(
    requests: &mut Vec<SavedRequest>,
    folders: &mut Vec<String>,
    folder: &str,
) -> Result<(), String> {
    if folder.is_empty() || !folders.iter().any(|existing| existing == folder) {
        return Err("待删除文件夹不存在，不能删除根目录。".into());
    }
    folders.retain(|existing| !in_folder(existing, folder));
    requests.retain(|request| !in_folder(&request.folder, folder));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, folder: &str) -> SavedRequest {
        let mut request = SavedRequest::new(name);
        request.folder = folder.into();
        request
    }

    #[test]
    fn moves_request_between_folders_and_reorders() {
        let a = request("a", "接口");
        let b = request("b", "接口/用户");
        let c = request("c", "接口/用户");
        let mut requests = vec![a.clone(), b.clone(), c.clone()];
        let mut folders = vec!["接口".into(), "接口/用户".into()];
        move_item(
            &mut requests,
            &mut folders,
            &TreeItem::Request(a.id.clone()),
            "接口/用户",
            Some(&TreeItem::Request(c.id.clone())),
        )
        .unwrap();
        assert_eq!(
            requests.iter().map(|r| &r.id).collect::<Vec<_>>(),
            vec![&b.id, &a.id, &c.id]
        );
        assert_eq!(requests[1].folder, "接口/用户");
        move_item(
            &mut requests,
            &mut folders,
            &TreeItem::Request(a.id.clone()),
            "",
            None,
        )
        .unwrap();
        assert!(
            requests
                .iter()
                .find(|r| r.id == a.id)
                .unwrap()
                .folder
                .is_empty()
        );
    }

    #[test]
    fn moving_folder_rewrites_entire_subtree_not_similar_prefixes() {
        let mut folders = vec!["a".into(), "a/child".into(), "ab".into(), "b".into()];
        let mut requests = vec![request("nested", "a/child"), request("other", "ab")];
        let result = move_item(
            &mut requests,
            &mut folders,
            &TreeItem::Folder("a".into()),
            "b",
            None,
        )
        .unwrap();
        assert_eq!(result, TreeItem::Folder("b/a".into()));
        assert!(folders.contains(&"b/a/child".into()));
        assert_eq!(requests[0].folder, "b/a/child");
        assert_eq!(requests[1].folder, "ab");
    }

    #[test]
    fn cycles_and_collisions_leave_data_unchanged() {
        let mut folders = vec!["a".into(), "a/child".into(), "b".into(), "b/a".into()];
        let mut requests = vec![request("one", "a/child")];
        let original = (requests.clone(), folders.clone());
        for destination in ["a", "a/child", "b"] {
            assert!(
                move_item(
                    &mut requests,
                    &mut folders,
                    &TreeItem::Folder("a".into()),
                    destination,
                    None
                )
                .is_err()
            );
            assert_eq!((requests.clone(), folders.clone()), original);
        }
    }

    #[test]
    fn folder_reorder_and_rename_preserve_subtree() {
        let mut folders = vec!["a".into(), "a/child".into(), "b".into()];
        let mut requests = vec![request("one", "a/child")];
        move_item(
            &mut requests,
            &mut folders,
            &TreeItem::Folder("b".into()),
            "",
            Some(&TreeItem::Folder("a".into())),
        )
        .unwrap();
        assert_eq!(folders, vec!["b", "a", "a/child"]);
        relocate_folder(&mut requests, &mut folders, "a", "renamed").unwrap();
        assert_eq!(folders, vec!["b", "renamed", "renamed/child"]);
        assert_eq!(requests[0].folder, "renamed/child");
    }

    #[test]
    fn deleting_folder_removes_only_its_descendants() {
        let mut folders = vec!["a".into(), "a/child".into(), "ab".into()];
        let mut requests = vec![request("one", "a/child"), request("two", "ab")];
        delete_folder(&mut requests, &mut folders, "a").unwrap();
        assert_eq!(folders, vec!["ab"]);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].name, "two");
    }
}
