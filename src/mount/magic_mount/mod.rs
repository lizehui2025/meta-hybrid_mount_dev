// src/mount/magic_mount/mod.rs

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
                log::debug!("File {} is removed via whiteout", self.path.display());
                Ok(())
            }
        }
    }
}

impl MagicMount {
    fn symlink(&self) -> Result<()> {
        if let Some(module_path) = &self.node.module_path {
            // 如果已经在 tmpfs 中，直接创建链接；否则报错（因为不能在 RO 分区创建）
            if !self.has_tmpfs {
                bail!("Cannot create symlink {} on read-only filesystem! Parent directory needs tmpfs.", self.path.display());
            }

            clone_symlink(module_path, &self.work_dir_path).with_context(|| {
                format!("Failed to clone symlink {} to {}", module_path.display(), self.work_dir_path.display())
            })?;
            
            MOUNTED_SYMBOLS_FILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        } else {
            bail!("System integrity error: Root symlink {} cannot be modified!", self.path.display());
        }
    }

    fn regular_file(&self) -> Result<()> {
        if self.node.module_path.is_none() {
            bail!("Root file {} cannot be regular_file mounted!", self.path.display());
        }

        let module_path = self.node.module_path.as_ref().unwrap();

        // 确定挂载目标：如果在 tmpfs 中，挂载到工作区节点；否则挂载到真实路径
        let target = if self.has_tmpfs {
            if !self.work_dir_path.exists() {
                fs::File::create(&self.work_dir_path)?;
            }
            &self.work_dir_path
        } else {
            &self.path
        };

        log::debug!("Binding module file: {} -> {}", module_path.display(), target.display());

        mount_bind(module_path, target).with_context(|| {
            format!("Bind mount failed: {} -> {}", module_path.display(), target.display())
        })?;

        // 强制设置为只读，防止模块修改系统关键文件
        if let Err(e) = mount_remount(target, MountFlags::RDONLY | MountFlags::BIND, "") {
            log::warn!("Failed to remount {} as RO: {}", target.display(), e);
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        if self.umount {
            let _ = send_umountable(target);
        }

        MOUNTED_FILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn directory(&mut self) -> Result<()> {
        // 核心修复：判定当前层级是否需要开启 tmpfs
        let mut tmpfs_needed = self.has_tmpfs;

        if !tmpfs_needed {
            // 只要满足以下任一条件，必须为该目录开启 tmpfs：
            // 1. 目录被标记为 .replace (全量替换)
            // 2. 该目录本身由模块提供 (module_path is Some)
            if self.node.replace || self.node.module_path.is_some() {
                tmpfs_needed = true;
            } else {
                // 3. 检查所有子项，如果子项有覆盖或新增，父项必须是 tmpfs
                for (name, child_node) in &self.node.children {
                    let real_path = self.path.join(name);
                    
                    let need = match child_node.file_type {
                        NodeFileType::Symlink => true, // 软链接必须在 tmpfs 中创建
                        NodeFileType::Whiteout => real_path.exists(), // 删除操作必须在 tmpfs 中记录
                        _ => {
                            // 修正点：如果子项由模块提供，必须触发 tmpfs
                            if child_node.module_path.is_some() {
                                true
                            } else if let Ok(meta) = real_path.symlink_metadata() {
                                // 文件类型不一致（如 目录变文件）
                                NodeFileType::from(meta.file_type()) != child_node.file_type
                            } else {
                                // 目标不存在（新增文件）
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

        if tmpfs_needed && !self.has_tmpfs {
            // 仅在首次从系统分区切换到 tmpfs 时创建骨架
            utils::tmpfs_skeleton(&self.path, &self.work_dir_path, &self.node)?;
            
            // 建立本地 bind 循环，为 mount_move 做准备
            mount_bind(&self.work_dir_path, &self.work_dir_path)?;
        }

        // 如果不是 replace 模式，需要镜像现有的系统文件
        if self.path.exists() && !self.node.replace {
            self.mount_path(tmpfs_needed)?;
        }

        // 递归处理子项
        for (name, node) in &self.node.children {
            if node.skip { continue; }
            
            Self::new(node, &self.path, &self.work_dir_path, tmpfs_needed, self.umount)
                .do_mount()
                .with_context(|| format!("Magic mount error at {}/{}", self.path.display(), name))?;
        }

        // 提交挂载：将 tmpfs 移动到真实系统位置
        if tmpfs_needed && !self.has_tmpfs {
            log::debug!("Committing Magic Mount: moving tmpfs to {}", self.path.display());
            
            mount_remount(&self.work_dir_path, MountFlags::RDONLY | MountFlags::BIND, "").ok();
            
            mount_move(&self.work_dir_path, &self.path).with_context(|| {
                format!("Failed to move Magic Mount tmpfs to {}", self.path.display())
            })?;

            mount_change(&self.path, MountPropagationFlags::PRIVATE).ok();

            #[cfg(any(target_os = "linux", target_os = "android"))]
            if self.umount {
                let _ = send_umountable(&self.path);
            }
        }

        Ok(())
    }

    fn mount_path(&mut self, has_tmpfs: bool) -> Result<()> {
        if !self.path.is_dir() { return Ok(()); }

        for entry in fs::read_dir(&self.path)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            
            // 如果该文件在模块中有对应节点，由 do_mount 处理，此处跳过
            if let Some(node) = self.node.children.remove(&name) {
                if node.skip { continue; }
                
                Self::new(&node, &self.path, &self.work_dir_path, has_tmpfs, self.umount)
                    .do_mount()?;
            } else if has_tmpfs {
                // 如果在 tmpfs 中且模块未修改此文件，则从原分区镜像过来
                mount_mirror(&self.path, &self.work_dir_path, &entry)?;
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
) -> Result<()>
where
    P: AsRef<Path>,
{
    if let Some(root) = collect_module_files(module_dir, extra_partitions, need_id)? {
        let tmp_root = tmp_path.as_ref();
        let tmp_dir = tmp_root.join("magic_work");
        ensure_dir_exists(&tmp_dir)?;

        mount(mount_source, &tmp_dir, "tmpfs", MountFlags::empty(), Some(std::ffi::CStr::from_bytes_with_nul(b"mode=0755\0").unwrap()))?;
        mount_change(&tmp_dir, MountPropagationFlags::PRIVATE).ok();

        let ret = MagicMount::new(&root, Path::new("/"), &tmp_dir, false, umount).do_mount();

        // 清理工作区
        unmount(&tmp_dir, UnmountFlags::DETACH).ok();
        fs::remove_dir_all(&tmp_dir).ok();

        log::info!("Magic Mount sequence complete. Files: {}, Symlinks: {}", 
            MOUNTED_FILES.load(std::sync::atomic::Ordering::Relaxed),
            MOUNTED_SYMBOLS_FILES.load(std::sync::atomic::Ordering::Relaxed));
        
        ret
    } else {
        log::info!("No modules qualified for Magic Mount, skipping.");
        Ok(())
    }
}
