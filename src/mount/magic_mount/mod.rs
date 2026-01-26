
// Copyright 2026 Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

mod utils;

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicU32,
};

use anyhow::{Context, Result, bail};
use rustix::mount::{
    MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_bind, mount_change, mount_move,
    mount_remount, unmount,
};

#[cfg(any(target_os = "linux", target_os = "android"))]
use crate::try_umount::send_umountable;
use crate::{
    mount::{
        magic_mount::utils::{clone_symlink, collect_module_files, mount_mirror},
        node::{Node, NodeFileType},
    },
    try_umount,
    utils::ensure_dir_exists,
};

static MOUNTED_FILES: AtomicU32 = AtomicU32::new(0);
static MOUNTED_SYMBOLS_FILES: AtomicU32 = AtomicU32::new(0);

struct MagicMount {
    node: Node,
    path: PathBuf,
    work_dir_path: PathBuf,
    has_tmpfs: bool,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    umount: bool,
}

impl MagicMount {
    fn new<P>(
        node: &Node,
        path: P,
        work_dir_path: P,
        has_tmpfs: bool,
        #[cfg(any(target_os = "linux", target_os = "android"))] umount: bool,
    ) -> Self
    where
        P: AsRef<Path>,
    {
        Self {
            node: node.clone(),
            path: path.as_ref().join(node.name.clone()),
            work_dir_path: work_dir_path.as_ref().join(node.name.clone()),
            has_tmpfs,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            umount,
        }
    }

    fn do_mount(&mut self) -> Result<()> {
        match self.node.file_type {
            NodeFileType::Symlink => self.symlink(),
            NodeFileType::RegularFile => self.regular_file(),
            NodeFileType::Directory => self.directory(),
            NodeFileType::Whiteout => {
                log::debug!("file {} is removed", self.path.display());
                Ok(())
            }
        }
    }
}

impl MagicMount {
    fn symlink(&self) -> Result<()> {
        if let Some(module_path) = &self.node.module_path {
            if !self.has_tmpfs {
                bail!("Cannot create symlink {} on read-only filesystem! Parent directory needs tmpfs.", self.path.display());
            }

            log::debug!(
                "create module symlink {} -> {}",
                module_path.display(),
                self.work_dir_path.display()
            );
            clone_symlink(module_path, &self.work_dir_path).with_context(|| {
                format!(
                    "create module symlink {} -> {}",
                    module_path.display(),
                    self.work_dir_path.display(),
                )
            })?;
            let mounted = MOUNTED_SYMBOLS_FILES.load(std::sync::atomic::Ordering::Relaxed) + 1;
            MOUNTED_SYMBOLS_FILES.store(mounted, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        } else {
            bail!("cannot mount root symlink {}!", self.path.display());
        }
    }

    fn regular_file(&self) -> Result<()> {
        let target = if self.has_tmpfs {
            if !self.work_dir_path.exists() {
                fs::File::create(&self.work_dir_path)?;
            }
            &self.work_dir_path
        } else {
            &self.path
        };

        if self.node.module_path.is_none() {
            bail!("cannot mount root file {}!", self.path.display());
        }

        let module_path = &self.node.module_path.clone().unwrap();

        log::debug!(
            "mount module file {} -> {}",
            module_path.display(),
            target.display() // use target display
        );

        mount_bind(module_path, target).with_context(|| {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if self.umount {
                let _ = send_umountable(target);
            }
            format!(
                "mount module file {} -> {}",
                module_path.display(),
                target.display(),
            )
        })?;

        if let Err(e) = mount_remount(target, MountFlags::RDONLY | MountFlags::BIND, "") {
            log::warn!("make file {} ro: {e:#?}", target.display());
        }

        let mounted = MOUNTED_FILES.load(std::sync::atomic::Ordering::Relaxed) + 1;
        MOUNTED_FILES.store(mounted, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn directory(&mut self) -> Result<()> {
        let mut tmpfs_needed = self.has_tmpfs;

        // 判定逻辑修复：如果不是已经在 tmpfs 中，需要判断是否要开启 tmpfs
        if !tmpfs_needed {
            // 条件1：全量替换或模块自带目录
            if self.node.replace || self.node.module_path.is_some() {
                tmpfs_needed = true;
            } else {
                // 条件2：子项目检查
                for (name, node) in &self.node.children {
                    let real_path = self.path.join(name);
                    let need = match node.file_type {
                        NodeFileType::Symlink => true, // 软链接必须 tmpfs
                        NodeFileType::Whiteout => real_path.exists(), // 删除必须 tmpfs
                        _ => {
                             // 子项是模块提供的，或者文件类型改变，或者新增文件 -> 必须 tmpfs
                            if node.module_path.is_some() {
                                true
                            } else if let Ok(metadata) = real_path.symlink_metadata() {
                                let file_type = NodeFileType::from(metadata.file_type());
                                file_type != node.file_type || file_type == NodeFileType::Symlink
                            } else {
                                // 目标不存在（新增）
                                true
                            }
                        }
                    };
                    if need {
                        tmpfs_needed = true;
                        break;
                    }
                }
            }
        }
        
        // 如果决定开启 tmpfs 但尚未开启（即当前层级是 tmpfs 的根）
        if tmpfs_needed && !self.has_tmpfs {
            utils::tmpfs_skeleton(&self.path, &self.work_dir_path, &self.node)?;
            
            // !!! 关键修复 !!!
            // 在执行 mount_move 之前，源路径必须是一个挂载点。
            // 对于普通目录，我们必须先执行一次 bind mount 自身。
            mount_bind(&self.work_dir_path, &self.work_dir_path).with_context(|| {
                format!(
                    "creating tmpfs (self-bind) for {} at {}",
                    self.path.display(),
                    self.work_dir_path.display(),
                )
            })?;
        }

        // 如果这不是 replace 模式，需要把原系统的文件镜像过来
        if self.path.exists() && !self.node.replace {
            self.mount_path(tmpfs_needed)?;
        }

        // 递归处理子节点
        for (name, node) in &self.node.children {
            if node.skip {
                continue;
            }

            if let Err(e) = {
                Self::new(
                    node,
                    &self.path,
                    &self.work_dir_path,
                    tmpfs_needed,
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    self.umount,
                )
                .do_mount()
            }
            .with_context(|| format!("magic mount {}/{name}", self.path.display()))
            {
                // 如果已经在 tmpfs 里了，子项失败是致命的
                if tmpfs_needed {
                    return Err(e);
                }
                log::error!("mount child {}/{name} failed: {e:#?}", self.path.display());
            }
        }

        // 提交挂载：将准备好的 tmpfs 移动到系统真实路径
        if tmpfs_needed && !self.has_tmpfs {
            log::debug!(
                "moving tmpfs {} -> {}",
                self.work_dir_path.display(),
                self.path.display()
            );

            // 设为只读
            if let Err(e) = mount_remount(
                &self.work_dir_path,
                MountFlags::RDONLY | MountFlags::BIND,
                "",
            ) {
                log::warn!("make dir {} ro: {e:#?}", self.path.display());
            }
            
            // 移动挂载点
            mount_move(&self.work_dir_path, &self.path).with_context(|| {
                format!(
                    "moving tmpfs {} -> {}",
                    self.work_dir_path.display(),
                    self.path.display()
                )
            })?;
            
            // 设为私有
            if let Err(e) = mount_change(&self.path, MountPropagationFlags::PRIVATE) {
                log::warn!("make dir {} private: {e:#?}", self.path.display());
            }

            #[cfg(any(target_os = "linux", target_os = "android"))]
            if self.umount {
                let _ = send_umountable(&self.path);
            }
        }
        Ok(())
    }
}

impl MagicMount {
    fn mount_path(&mut self, has_tmpfs: bool) -> Result<()> {
        // 如果路径不存在或者是文件，不需要遍历
        if !self.path.is_dir() {
            return Ok(()); 
        }

        for entry in self.path.read_dir()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            
            // 如果该文件由模块提供，前面递归逻辑会处理，这里跳过
            // 如果模块没有提供该文件，但我们需要 tmpfs（即 mirror），则执行 mount_mirror
            let result = {
                if let Some(node) = self.node.children.remove(&name) {
                    if node.skip {
                        continue;
                    }
                    // 这里不需要由 mount_path 调用递归 do_mount，
                    // 因为 directory() 主逻辑里的循环会处理 self.node.children。
                    // 但是因为我们在这里 remove 了它，所以必须在这里处理，或者改写逻辑。
                    // 保持原逻辑结构：在这里处理并移除，防止 directory() 尾部重复处理？
                    // 不，directory() 的循环在 mount_path 之后。
                    // 如果在这里处理了，directory() 里的循环就拿不到 node 了（因为 remove 了）。
                    // 这也是原代码的设计：优先处理系统里已存在的文件。
                    
                    Self::new(
                        &node,
                        &self.path,
                        &self.work_dir_path,
                        has_tmpfs,
                        #[cfg(any(target_os = "linux", target_os = "android"))]
                        self.umount,
                    )
                    .do_mount()
                    .with_context(|| format!("magic mount {}/{name}", self.path.display()))
                } else if has_tmpfs {
                    mount_mirror(&self.path, &self.work_dir_path, &entry)
                        .with_context(|| format!("mount mirror {}/{name}", self.path.display()))
                } else {
                    Ok(())
                }
            };

            if let Err(e) = result {
                if has_tmpfs {
                    return Err(e);
                }
                log::error!("mount child {}/{name} failed: {e:#?}", self.path.display());
            }
        }

        Ok(())
    }
}

pub fn magic_mount<P>(
    tmp_path: P,
    module_dir: &Path,
    mount_source: &str,
    extra_partitions: &[String],
    need_id: HashSet<String>,
    #[cfg(any(target_os = "linux", target_os = "android"))] umount: bool,
    #[cfg(not(any(target_os = "linux", target_os = "android")))] _umount: bool,
) -> Result<()>
where
    P: AsRef<Path>,
{
    if let Some(root) = collect_module_files(module_dir, extra_partitions, need_id)? {
        log::debug!("collected: {root:?}");
        let tmp_root = tmp_path.as_ref();
        let tmp_dir = tmp_root.join("workdir");
        ensure_dir_exists(&tmp_dir)?;

        mount(mount_source, &tmp_dir, "tmpfs", MountFlags::empty(), None).context("mount tmp")?;
        mount_change(&tmp_dir, MountPropagationFlags::PRIVATE).context("make tmp private")?;

        #[cfg(any(target_os = "linux", target_os = "android"))]
        if umount {
            let _ = send_umountable(&tmp_dir);
        }

        let ret = MagicMount::new(
            &root,
            Path::new("/"),
            tmp_dir.as_path(),
            false,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            umount,
        )
        .do_mount();

        if let Err(e) = unmount(&tmp_dir, UnmountFlags::DETACH) {
            log::error!("failed to unmount tmp {e}");
        }
        #[cfg(any(target_os = "android", target_os = "linux"))]
        try_umount::commit()?;
        fs::remove_dir(tmp_dir).ok();

        let mounted_symbols = MOUNTED_SYMBOLS_FILES.load(std::sync::atomic::Ordering::Relaxed);
        let mounted_files = MOUNTED_FILES.load(std::sync::atomic::Ordering::Relaxed);
        log::info!("mounted files: {mounted_files}, mounted symlinks: {mounted_symbols}");
        ret
    } else {
        log::info!("no modules to mount, skipping!");
        Ok(())
    }
}
