import type { AppConfig, Module, StorageStatus, SystemInfo, DeviceInfo, ModuleRules, ConflictEntry, DiagnosticIssue, HymoStatus } from './types';
import { DEFAULT_CONFIG } from './constants';

const MOCK_DELAY = 500;
const delay = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

let mockConfig: AppConfig = { ...DEFAULT_CONFIG };

// Mock Hymo Status
let mockHymoStatus: HymoStatus = {
  available: true,
  protocol_version: 5,
  config_version: 1,
  stealth_active: true,
  debug_active: false,
  rules: {
    redirects: [
      { src: "/system/fonts/Roboto-Regular.ttf", target: "/data/local/tmp/font_override.ttf", type: 0 },
      { src: "/vendor/etc/mixer_paths.xml", target: "/data/adb/modules/sound_mod/mixer_paths.xml", type: 0 }
    ],
    hides: [
      "/system/xbin/su",
      "/data/adb/magisk"
    ],
    injects: [
      "/system/etc/security/cacerts"
    ],
    xattr_sbs: [
      "0xffff12345678"
    ]
  }
};

export const MockAPI = {
  loadConfig: async (): Promise<AppConfig> => {
    await delay(MOCK_DELAY);
    return { ...mockConfig };
  },
  saveConfig: async (config: AppConfig): Promise<void> => {
    await delay(MOCK_DELAY);
    mockConfig = { ...config };
    console.log("[Mock] Config saved:", config);
  },
  resetConfig: async (): Promise<void> => {
    await delay(MOCK_DELAY);
    mockConfig = { ...DEFAULT_CONFIG };
    console.log("[Mock] Config reset");
  },
  scanModules: async (path?: string): Promise<Module[]> => {
    await delay(MOCK_DELAY);
    return [
      { 
        id: "magisk_module_1", 
        name: "Test Module", 
        version: "1.0", 
        author: "Dev", 
        description: "A test module", 
        mode: "auto", 
        is_mounted: true,
        rules: { default_mode: 'overlay', paths: {} }
      },
      { 
        id: "hymofs_module", 
        name: "HymoFS Test", 
        version: "2.0", 
        author: "Dev", 
        description: "Testing HymoFS injection", 
        mode: "hymofs", 
        is_mounted: true,
        rules: { default_mode: 'hymofs', paths: {} }
      }
    ];
  },
  saveModuleRules: async (moduleId: string, rules: ModuleRules): Promise<void> => {
    await delay(MOCK_DELAY);
    console.log(`[Mock] Rules saved for ${moduleId}:`, rules);
  },
  saveModules: async (modules: Module[]): Promise<void> => {
    await delay(MOCK_DELAY);
    console.log("[Mock] Modules saved (reordered)");
  },
  readLogs: async (logPath?: string, lines = 1000): Promise<string> => {
    await delay(MOCK_DELAY);
    return `[INFO] Daemon started\n[INFO] Mock logs content here...\n[WARN] This is a mock warning`;
  },
  getStorageUsage: async (): Promise<StorageStatus> => {
    await delay(MOCK_DELAY);
    return {
      size: "128 MB",
      used: "32 MB",
      percent: "25%",
      type: "ext4",
      hymofs_available: true
    };
  },
  getSystemInfo: async (): Promise<SystemInfo> => {
    await delay(MOCK_DELAY);
    return {
      kernel: "5.10.100-android12-mock",
      selinux: "Enforcing",
      mountBase: "/data/adb/meta-hybrid/mnt",
      activeMounts: ["system", "vendor"],
      zygisksuEnforce: "1"
    };
  },
  getDeviceStatus: async (): Promise<DeviceInfo> => {
    await delay(MOCK_DELAY);
    return {
      model: "Pixel 7 Pro (Mock)",
      android: "14 (API 34)",
      kernel: "5.10.0-mock",
      selinux: "Enforcing"
    };
  },
  getVersion: async (): Promise<string> => {
    await delay(MOCK_DELAY);
    return "v1.0.0-MOCK";
  },
  openLink: async (url: string): Promise<void> => {
    console.log("[Mock] Open link:", url);
    window.open(url, '_blank');
  },
  fetchSystemColor: async (): Promise<string | null> => {
    return "#6750a4";
  },
  getConflicts: async (): Promise<ConflictEntry[]> => {
    await delay(MOCK_DELAY);
    return [
      {
        partition: "system",
        relative_path: "fonts/Roboto-Regular.ttf",
        contending_modules: ["font_mod_a", "font_mod_b"]
      }
    ];
  },
  getDiagnostics: async (): Promise<DiagnosticIssue[]> => {
    await delay(MOCK_DELAY);
    return [
       { level: 'Info', context: 'Environment', message: 'Mock Environment Detected' },
       { level: 'Warning', context: 'Storage', message: 'Storage usage > 80% (Mock)' }
    ];
  },
  reboot: async (): Promise<void> => {
    console.log("[Mock] Reboot requested");
  },
  getHymoStatus: async (): Promise<HymoStatus> => {
    await delay(MOCK_DELAY);
    return { ...mockHymoStatus };
  },
  setHymoStealth: async (enable: boolean): Promise<void> => {
    await delay(200);
    mockHymoStatus.stealth_active = enable;
    console.log("[Mock] Hymo Stealth:", enable);
  },
  setHymoDebug: async (enable: boolean): Promise<void> => {
    await delay(200);
    mockHymoStatus.debug_active = enable;
    console.log("[Mock] Hymo Debug:", enable);
  },
  triggerMountReorder: async (): Promise<void> => {
    await delay(500);
    console.log("[Mock] Mounts reordered");
  }
};