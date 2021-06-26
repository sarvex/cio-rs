use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::ops::Add;
use std::path::{Path, PathBuf};
use std::str::from_utf8;
use std::thread;
use std::time;

use futures_util::stream::TryStreamExt;
use hubcaps::issues::Issue;
use hubcaps::repositories::Repository;
use reqwest::get;
use serde_json::Value;

/// Write a file.
pub fn write_file(file: &Path, contents: &str) {
    // create each directory.
    fs::create_dir_all(file.parent().unwrap()).unwrap();

    // Write to the file.
    let mut f = fs::File::create(file.to_path_buf()).unwrap();
    f.write_all(contents.as_bytes()).unwrap();

    println!("wrote file: {}", file.to_str().unwrap());
}

/// Check if a GitHub issue already exists.
pub fn check_if_github_issue_exists(issues: &[Issue], search: &str) -> Option<Issue> {
    for i in issues {
        if i.title.contains(search) {
            return Some(i.clone());
        }
    }

    None
}

/// Return a user's public ssh key's from GitHub by their GitHub handle.
pub async fn get_github_user_public_ssh_keys(handle: &str) -> Vec<String> {
    let body = get(&format!("https://github.com/{}.keys", handle)).await.unwrap().text().await.unwrap();

    body.lines()
        .filter_map(|key| {
            let kt = key.trim();
            if !kt.is_empty() {
                Some(kt.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Get a files content from a repo.
/// It returns a tuple of the bytes of the file content and the sha of the file.
pub async fn get_file_content_from_repo(repo: &Repository, branch: &str, path: &str) -> (Vec<u8>, String) {
    // Add the starting "/" so this works.
    // TODO: figure out why it doesn't work without it.
    let mut file_path = path.to_string();
    if !path.starts_with('/') {
        file_path = "/".to_owned() + path;
    }

    // Try to get the content for the file from the repo.
    match repo.content().file(&file_path, branch).await {
        Ok(file) => return (file.content.into(), file.sha),
        Err(e) => {
            match e {
                hubcaps::errors::Error::RateLimit { reset } => {
                    // We got a rate limit error.
                    println!("got rate limited, sleeping for {}s", reset.as_secs());
                    thread::sleep(reset.add(time::Duration::from_secs(5)));
                }
                hubcaps::errors::Error::Fault { code: _, ref error } => {
                    if error.message.contains("too large") {
                        // The file is too big for us to get it's contents through this API.
                        // The error suggests we use the Git Data API but we need the file sha for
                        // that.
                        // Get all the items in the directory and try to find our file and get the sha
                        // for it so we can update it.
                        let mut path = PathBuf::from(&file_path);
                        path.pop();

                        for item in repo.content().iter(path.to_str().unwrap(), branch).try_collect::<Vec<hubcaps::content::DirectoryItem>>().await.unwrap() {
                            if file_path.trim_start_matches('/') != item.path {
                                // Continue early.
                                continue;
                            }

                            // Otherwise, this is our file.
                            // We have the sha we can see if the files match using the
                            // Git Data API.
                            let blob = repo.git().blob(&item.sha).await.unwrap();
                            // Base64 decode the contents.
                            // TODO: move this logic to hubcaps.
                            let v = blob.content.replace("\n", "");
                            let decoded = base64::decode_config(&v, base64::STANDARD).unwrap();
                            return (decoded.trim(), item.sha.to_string());
                        }
                    }

                    println!("[github content] Getting the file at {} on branch {} failed: {:?}", file_path, branch, e);
                }
                _ => {
                    println!("[github content] Getting the file at {} on branch {} failed: {:?}", file_path, branch, e);
                }
            }
        }
    }

    // By default return nothing. This only happens if we could not get the file for some reason.
    (vec![], "".to_string())
}

/// Create or update a file in a GitHub repository.
/// If the file does not exist, it will be created.
/// If the file exists, it will be updated _only if_ the content of the file has changed.
pub async fn create_or_update_file_in_github_repo(repo: &Repository, branch: &str, path: &str, new_content: Vec<u8>) {
    let content = new_content.trim();
    // Add the starting "/" so this works.
    // TODO: figure out why it doesn't work without it.
    let mut file_path = path.to_string();
    if !path.starts_with('/') {
        file_path = "/".to_owned() + path;
    }

    // Try to get the content for the file from the repo.
    let (existing_content, sha) = get_file_content_from_repo(repo, branch, path).await;

    if !existing_content.is_empty() || !sha.is_empty() {
        if content == existing_content {
            // They are the same so we can return early, we do not need to update the
            // file.
            println!("[github content] File contents at {} are the same, no update needed", file_path);
            return;
        }

        // When the pdfs are generated they change the modified time that is
        // encoded in the file. We want to get that diff and see if it is
        // the only change so that we are not always updating those files.
        let diff = diffy::create_patch_bytes(&existing_content, &content);
        let bdiff = diff.to_bytes();
        let str_diff = from_utf8(&bdiff).unwrap_or("");
        if str_diff.contains("-/ModDate") && str_diff.contains("-/CreationDate") && str_diff.contains("+/ModDate") && str_diff.contains("-/CreationDate") && str_diff.contains("@@ -5,8 +5,8 @@") {
            // The binary contents are the same so we can return early.
            // The only thing that changed was the modified time and creation date.
            println!("[github content] File contents at {} are the same, no update needed", file_path);
            return;
        }

        // We need to update the file. Ignore failure.
        match repo
            .content()
            .update(
                &file_path,
                &content,
                &format!(
                    "Updating file content {} programatically\n\nThis is done from the cio repo utils::create_or_update_file function.",
                    file_path
                ),
                &sha,
                branch,
            )
            .await
        {
            Ok(_) => (),
            Err(e) => {
                println!("[github content] updating file at {} on branch {} failed: {}", file_path, branch, e);
                return;
            }
        }

        println!("[github content] Updated file at {}", file_path);
        return;
    }

    // Create the file in the repo. Ignore failure.
    match repo
        .content()
        .create(
            &file_path,
            &content,
            &format!(
                "Creating file content {} programatically\n\nThis is done from the cio repo utils::create_or_update_file function.",
                file_path
            ),
            branch,
        )
        .await
    {
        Ok(_) => (),
        Err(e) => {
            println!("[github content] creating file at {} on branch {} failed: {}", file_path, branch, e);
            return;
        }
    }

    println!("[github content] Created file at {}", file_path);
}

trait SliceExt {
    fn trim(&self) -> Self;
}

impl SliceExt for Vec<u8> {
    fn trim(&self) -> Vec<u8> {
        fn is_whitespace(c: &u8) -> bool {
            c == &b'\t' || c == &b' '
        }

        fn is_not_whitespace(c: &u8) -> bool {
            !is_whitespace(c)
        }

        if let Some(first) = self.iter().position(is_not_whitespace) {
            if let Some(last) = self.iter().rposition(is_not_whitespace) {
                self[first..last + 1].to_vec()
            } else {
                unreachable!();
            }
        } else {
            vec![]
        }
    }
}

pub fn default_date() -> chrono::naive::NaiveDate {
    chrono::naive::NaiveDate::parse_from_str("1970-01-01", "%Y-%m-%d").unwrap()
}

pub fn merge_json(a: &mut Value, b: Value) {
    match (a, b) {
        (a @ &mut Value::Object(_), Value::Object(b)) => {
            let a = a.as_object_mut().unwrap();
            for (k, v) in b {
                merge_json(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (a @ &mut Value::Array(_), Value::Array(b)) => {
            let a = a.as_array_mut().unwrap();
            for v in b {
                a.push(v);
            }
        }
        (a, b) => *a = b,
    }
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((idx, _)) => s[..idx].to_string(),
    }
}

pub fn get_value(map: &HashMap<String, Vec<String>>, key: &str) -> String {
    let empty: Vec<String> = Default::default();
    let a = map.get(key).unwrap_or(&empty);

    if a.is_empty() {
        return Default::default();
    }

    a.get(0).unwrap().to_string()
}
