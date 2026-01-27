// Copyright 2026 Hybrid Mount Developers
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    conf::config,
    core::planner::{MountPlan, OverlayOperation},
    defs,
    mount::{magic_mount, overlayfs},
    utils,
};

/// 执行结果汇总
pub struct ExecutionResult {
    pub overlay_module_ids: Vec<String>,
    pub magic_module_ids: Vec<String>,
}

/// 挂载事务守卫：负责记录并在必要时回滚挂载操作
struct MountTransaction {
    mounted_targets: Vec<String>,
}

impl MountTransaction {
    fn new() -> Self {
        Self { mounted_targets: Vec::new() }
    }

    /// 记录一个新的成功挂载
    fn register(&mut self, target: &str) {
        self.mounted_targets.push(target.to_string());
    }

    /// 执行回滚：按挂载相反的顺序强制卸载
    fn rollback(self) {
        if self.mounted_targets.is_empty() { return; }
        log::warn!("Rolling back {} mount points due to inconsistency...", self.mounted_targets.len());
        
        for target in self.mounted_targets.into_iter().rev() {
            // 这里调用底层的卸载逻辑，建议在 utils 中实现一个通用的强制卸载
            let _ = crate::try_umount::send_umountable(&target);
        }
    }
}

pub fn execute(plan: &MountPlan, config: &config::Config) -> Result<ExecutionResult> {
    log::info!(">> Link Start! Robust execution sequence initiated.");

    // 全局事务管理器，负责最终的挂载生命周期
    let mut global_tx = MountTransaction::new();
    
    // 记录由于 OverlayFS 失败而需要转入 Magic 模式的模块
    let mut final_magic_ids: HashSet<String> = plan.magic_module_ids.iter().cloned().collect();
    let mut final_overlay_ids = HashSet::new();

    // 映射表：模块 ID -> 该模块涉及的所有挂载目标 (用于一致性回滚)
    let mut module_to_targets: HashMap<String, Vec<String>> = HashMap::new();

    log::info!(">> Phase 1: Contextual OverlayFS Execution...");

    // 1. 尝试执行所有 Overlay 挂载
    for op in &plan.overlay_ops {
        let involved_modules: Vec<String> = op.lowerdirs.iter()
            .filter_map(|p| utils::extract_module_id(p))
            .collect();

        match try_perform_overlay_mount(op, config) {
            Ok(_) => {
                global_tx.register(&op.target);
                // 记录模块与挂载点的关联
                for id in &involved_modules {
                    module_to_targets.entry(id.clone()).or_default().push(op.target.clone());
                    final_overlay_ids.insert(id.clone());
                }
            }
            Err(e) => {
                log::warn!("OverlayFS failure at {}: {}. Module-level fallback triggered.", op.target, e);
                // 该分区涉及的所有模块都必须标记为 Magic 模式
                for id in involved_modules {
                    final_magic_ids.insert(id);
                }
            }
        }
    }

    // 2. 一致性检查：如果一个模块被标记为 Magic，撤销它所有已成功的 Overlay 挂载
    // 这是为了解决“半 Overlay, 半 Magic”的问题
    let mut inconsistent_targets = Vec::new();
    final_overlay_ids.retain(|id| {
        if final_magic_ids.contains(id) {
            if let Some(targets) = module_to_targets.remove(id) {
                inconsistent_targets.extend(targets);
            }
            false // 从 Overlay 列表移除
        } else {
            true
        }
    });

    if !inconsistent_targets.is_empty() {
        inconsistent_targets.sort();
        inconsistent_targets.dedup();
        for target in inconsistent_targets {
            log::info!("Cleaning up inconsistent overlay mount: {}", target);
            let _ = crate::try_umount::send_umountable(&target);
        }
    }

    // 3. 执行 Magic Mount (Phase 2)
    let mut magic_queue: Vec<String> = final_magic_ids.iter().cloned().collect();
    magic_queue.sort();

    if !magic_queue.is_empty() {
        let tempdir = PathBuf::from(&config.hybrid_mnt_dir).join("magic_workspace");
        log::info!(">> Phase 2: Magic Mount Execution (Fallback/Native) at {}", tempdir.display());

        if !tempdir.exists() {
            std::fs::create_dir_all(&tempdir).context("Failed to create magic workspace")?;
        }

        let module_dir = Path::new(&config.hybrid_mnt_dir);
        let magic_need_set: HashSet<String> = magic_queue.iter().cloned().collect();

        // Magic Mount 失败现在被视为严重错误，会中断流程
        crate::mount::magic_mount::magic_mount(
            &tempdir,
            module_dir,
            &config.mountsource,
            &config.partitions,
            magic_need_set,
            !config.disable_umount,
        ).context("Critical failure during Magic Mount phase")?;
        
        let _ = crate::try_umount::TMPFS.set(tempdir.to_string_lossy().to_string());
    }

    // 4. 提交卸载任务
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if !config.disable_umount {
        crate::try_umount::commit().map_err(|e| {
            log::error!("Final try_umount commit failed: {}", e);
            e
        })?;
    }

    let mut result_overlay: Vec<String> = final_overlay_ids.into_iter().collect();
    let mut result_magic: Vec<String> = final_magic_ids.into_iter().collect();
    result_overlay.sort();
    result_magic.sort();

    Ok(ExecutionResult {
        overlay_module_ids: result_overlay,
        magic_module_ids: result_magic,
    })
}

/// 内部辅助函数：执行具体的 Overlay 挂载
fn try_perform_overlay_mount(op: &OverlayOperation, config: &config::Config) -> Result<()> {
    let lowerdir_strings: Vec<String> = op.lowerdirs.iter()
        .map(|p| p.display().to_string())
        .collect();

    let rw_root = Path::new(defs::SYSTEM_RW_DIR);
    let part_rw = rw_root.join(&op.partition_name);
    let upper = part_rw.join("upperdir");
    let work = part_rw.join("workdir");

    let (upper_opt, work_opt) = if upper.exists() && work.exists() {
        (Some(upper), Some(work))
    } else {
        (None, None)
    };

    log::info!("Mounting {} [OVERLAY] ({} layers)", op.target, lowerdir_strings.len());

    overlayfs::overlayfs::mount_overlay(
        &op.target,
        &lowerdir_strings,
        work_opt,
        upper_opt,
        &config.mountsource,
    ).map_err(|e| anyhow::anyhow!(e))?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    if !config.disable_umount {
        crate::try_umount::send_umountable(&op.target)?;
    }

    Ok(())
}
