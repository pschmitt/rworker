use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use crate::ctx::{self, Ctx, Fail, R};
use crate::proc;

const DEFAULT_FLAKE: &str = "github:pschmitt/rworker";
const DEFAULT_IMAGE: &str = "nixosConfigurations.rworker.config.image.repart.image";

fn usage_error(ctx: &Ctx, message: &str) -> Fail {
    ctx.err(message);
    crate::help::show(ctx, "image");
    Fail(2)
}

fn image_error(ctx: &Ctx, message: &str) -> Fail {
    ctx.err(message);
    Fail(1)
}

fn disk_format(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "raw" | "img" => Some("raw"),
        "qcow2" => Some("qcow2"),
        _ => None,
    }
}

fn build(ctx: &Ctx, flake: &str) -> R<PathBuf> {
    let installable = if flake.contains('#') {
        flake.to_owned()
    } else {
        format!("{flake}#{DEFAULT_IMAGE}")
    };
    let (code, output) = proc::capture(
        ctx,
        Command::new("nix").args([
            "build",
            "--no-link",
            "--print-out-paths",
            "--",
            &installable,
        ]),
    )?;
    if code != 0 {
        return Err(Fail(code));
    }
    let mut images = Vec::new();
    for path in output.lines().map(Path::new) {
        if path.is_dir() {
            let entries = fs::read_dir(path)
                .map_err(|error| image_error(ctx, &format!("Read {}: {error}", path.display())))?;
            for entry in entries {
                let entry = entry.map_err(|error| image_error(ctx, &error.to_string()))?;
                let path = entry.path();
                if path.is_file() && disk_format(&path).is_some() {
                    images.push(path);
                }
            }
        } else if path.is_file() && disk_format(path).is_some() {
            images.push(path.to_owned());
        }
    }
    if images.len() != 1 {
        return Err(image_error(
            ctx,
            &format!(
                "Expected exactly one .raw, .img, or .qcow2 image from {installable}, found {}",
                images.len()
            ),
        ));
    }
    Ok(images.remove(0))
}

#[derive(Default)]
struct UploadOptions<'a> {
    name: Option<&'a str>,
    tag: Option<&'a str>,
    cloud: Option<&'a str>,
    project: Option<&'a str>,
}

fn upload(ctx: &Ctx, image: &Path, name: &str, options: &UploadOptions<'_>) -> R<String> {
    let Some(format) = disk_format(image) else {
        return Err(image_error(
            ctx,
            "Image extension must be .raw, .img, or .qcow2",
        ));
    };
    if !image.is_file() {
        return Err(image_error(
            ctx,
            &format!("Image file is missing: {}", image.display()),
        ));
    }
    let mut command = Command::new("openstack");
    if let Some(cloud) = options.cloud {
        command.args(["--os-cloud", cloud]);
    }
    command
        .args(["image", "create", "--file"])
        .arg(image)
        .args([
            "--disk-format",
            format,
            "--container-format",
            "bare",
            "--private",
            "--property",
            "hw_firmware_type=uefi",
            "--property",
            "hw_machine_type=q35",
            "--property",
            "hw_disk_bus=virtio",
            "--property",
            "os_type=linux",
            "-f",
            "value",
            "-c",
            "id",
        ]);
    if let Some(tag) = options.tag {
        command.args(["--tag", tag]);
    }
    if let Some(project) = options.project {
        command.args(["--project", project]);
    }
    let (code, id) = proc::capture(ctx, command.args(["--", name]))?;
    if code != 0 {
        return Err(Fail(code));
    }
    if id.trim().is_empty() {
        return Err(image_error(ctx, "OpenStack returned no image ID"));
    }
    Ok(id.trim().to_owned())
}

pub fn run(ctx: &Ctx, argv: &[String]) -> R {
    let Some((action, rest)) = argv.split_first() else {
        return Err(usage_error(ctx, "Choose image create or image upload"));
    };
    let create = match action.as_str() {
        prefix if !prefix.is_empty() && "create".starts_with(prefix) => true,
        prefix if !prefix.is_empty() && "upload".starts_with(prefix) => false,
        _ => return Err(usage_error(ctx, "Choose image create or image upload")),
    };
    let (mut flake, mut path) = (None, None);
    let mut options = UploadOptions::default();
    let mut publish = !create;
    let mut args = rest.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        let target = match arg {
            "--flake" if create => &mut flake,
            "--name" => &mut options.name,
            "--tag" => &mut options.tag,
            "--cloud" => &mut options.cloud,
            "--project" => &mut options.project,
            "--upload" if create => {
                publish = true;
                continue;
            }
            "--" if !create && path.is_none() => {
                path = args.next();
                if path.is_none() || args.next().is_some() {
                    return Err(usage_error(ctx, "Expected one IMAGE path"));
                }
                break;
            }
            _ if !create && !arg.starts_with('-') && path.is_none() => {
                path = Some(arg);
                continue;
            }
            _ => {
                return Err(usage_error(
                    ctx,
                    &format!("Unexpected image argument: {arg}"),
                ));
            }
        };
        let value = args
            .next()
            .filter(|value| !value.trim().is_empty() && !value.starts_with("--"))
            .ok_or_else(|| usage_error(ctx, &format!("{arg} needs a value")))?;
        if target.replace(value).is_some() {
            return Err(usage_error(
                ctx,
                &format!("{arg} was specified more than once"),
            ));
        }
    }

    if !publish
        && (options.name.is_some()
            || options.tag.is_some()
            || options.cloud.is_some()
            || options.project.is_some())
    {
        return Err(usage_error(
            ctx,
            "--name, --tag, --cloud, and --project require --upload with image create",
        ));
    }
    let image = if create {
        build(ctx, flake.unwrap_or(DEFAULT_FLAKE))?
    } else {
        if options.name.is_none() {
            return Err(usage_error(ctx, "image upload needs --name"));
        }
        PathBuf::from(path.ok_or_else(|| usage_error(ctx, "image upload needs an IMAGE path"))?)
    };
    if publish {
        let name = options
            .name
            .or_else(|| image.file_stem().and_then(|value| value.to_str()))
            .ok_or_else(|| image_error(ctx, "Specify --name for this image"))?;
        let id = upload(ctx, &image, name, &options)?;
        if ctx.json.get() {
            ctx::json_out(
                ctx,
                &json!({"path": image, "name": name, "id": id}).to_string(),
            );
        } else {
            ctx::outln(&format!("Uploaded {name} ({id})"));
        }
    } else if ctx.json.get() {
        ctx::json_out(ctx, &json!({"path": image}).to_string());
    } else {
        ctx::outln(&image.display().to_string());
    }
    Ok(())
}
