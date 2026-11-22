import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowUpRight,
  CircleHelp,
  File,
  Folder,
  FolderOpen,
  Layers,
  LoaderCircle,
  RefreshCw,
  Square,
  Trash2,
  X,
} from "lucide-react";
import { Button } from "./components/ui/button";
import { AnimatedSegmentedControl } from "./components/ui/animated-segmented-control";
import { SpaceSelection } from "./components/SpaceSelection";
import { TrashAction } from "./components/TrashAction";
import { motion, useReducedMotion } from "framer-motion";
import { Checkbox } from "./components/ui/checkbox";
import { WorkspaceSearch } from "./components/WorkspaceSearch";
import { NativeSelect } from "./components/ui/native-select";
import {
  Dialog,
  DialogContent,
  DialogBody,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./components/ui/dialog";
import {
  diskOverview,
  startDiskScan,
  getDiskScan,
  getDiskScanView,
  inspectDiskDirectory,
  getCachedDiskScan,
  cancelDiskScan,
  diskInvoke,
  type DiskEntry,
  type DiskOverview,
  type DiskScan,
  type DiskScanMode,
  type ApplicationSearchResult,
  type DiskApplication,
} from "./lib/disk-api";
import { useI18n } from "./lib/i18n";
import { needsScanRefresh, readScanFreshness, recordScanRefresh, SCAN_FRESHNESS_KEY } from "./lib/scan-freshness";

import { formatBytes } from "./lib/utils";
import { stopTreemapTransition, transitionTreemap } from "./lib/treemap-transition";
import DiskTreemap from "./components/DiskTreemap";
import { PermissionOnboarding } from "./components/PermissionOnboarding";
import {
  getDiskPermissions,
  readPermissionChoice,
  PERMISSION_CHOICE_KEY,
  type PermissionChoice,
  type DiskPermissionInfo,
} from "./lib/permissions";
import { DesktopShell, type DesktopPage } from "./components/DesktopShell";
import { SettingsPanel } from "./components/SettingsPanel";
import { AppIcon } from "./components/AppIcon";
import { AppUninstallDialog } from "./components/AppUninstallDialog";
import { DirectoryContents } from "./components/DirectoryContents";
import { IconButton } from "./components/ui/icon-button";
import { TooltipProvider } from "./components/ui/tooltip";
import "./mac.css";
import "./utility.css";
import "./app-uninstall.css";
const CodexTools = lazy(() => import("./App"));
const ScanComparison = lazy(() => import("./components/ScanComparison").then(module => ({ default: module.ScanComparison })));
const AIConnection = lazy(() => import("./components/AIConnection"));
type ScanSource = Pick<DiskScan, "scanId" | "roots"> & { allowProtected: boolean };
const APP_UNINSTALL_LIMIT = 20;
const insideRoot = (path: string, root: string) => path === root || path.startsWith(root.endsWith("/") ? root : `${root}/`);
const folderName = (path: string) => path.split("/").filter(Boolean).at(-1) ?? path;
const scanBusy = (error: unknown) => String(error).includes("A disk scan is already running");
const palette = [
  "#d19050",
  "#689c8c",
  "#858db3",
  "#a383a9",
  "#a3a072",
  "#728fa5",
  "#b77f76",
  "#7d9980",
];
export default function MacApp() {
  const { locale, setLocale } = useI18n();
  const reducedMotion = useReducedMotion();

  const tr = (zh: string, en: string) => (locale === "zh" ? zh : en);
  const labels: Record<DesktopPage, string> = {
    explorer: tr("空间分析", "Space explorer"),
    applications: tr("应用占用", "Applications"),
    codex: tr("Codex 专项", "Codex tools"),
    ai: tr("AI 连接", "AI connections"),
    settings: tr("设置", "Settings"),
  };
  const [page, setPage] = useState<DesktopPage>("explorer");
  const [treemapHeight, setTreemapHeight] = useState(360);
  const stageRef = useRef<HTMLDivElement>(null);
  const setStage = useCallback((stage: HTMLDivElement | null) => {
    stageRef.current = stage;
    if (!stage) stopTreemapTransition();
  }, []);
  const [explorerMetric, setExplorerMetric] = useState<"allocation" | "growth">("allocation");
  const [explorerLayout, setExplorerLayout] = useState<"map" | "list">("map");
  const [selectedPath, setSelectedPath] = useState<string>();
  const [panelSide, setPanelSide] = useState<"left" | "right">("right");
  const [directoryDetailsOpen, setDirectoryDetailsOpen] = useState(false);
  const currentLocationRef = useRef<HTMLSpanElement>(null);
  const focusLocation = useRef(false);
  const [openingPath, setOpeningPath] = useState<string>();
  const previousSources = useRef<ScanSource[]>([]);
  const [initialFreshness] = useState(() => {
    try { return readScanFreshness(window.localStorage.getItem(SCAN_FRESHNESS_KEY)); }
    catch { return readScanFreshness(null); }
  });
  const scanFreshness = useRef(initialFreshness);
  const sourceProtection = useRef(false);
  const [permissionChoice, setPermissionChoice] =
    useState<PermissionChoice | null>(readPermissionChoice);
  const [permissionOpen, setPermissionOpen] = useState(false);
  const [checkingPermission, setCheckingPermission] = useState(true);
  const [requestedFull, setRequestedFull] = useState(false);
  const permissionRequest = useRef(0);
  const initialPermissionDecision = useRef(false);
  const permissionRef = useRef<DiskPermissionInfo | null>(null);
  const [scanDetailsOpen, setScanDetailsOpen] = useState(false);
  const [permissionInfo, setPermissionInfo] =
    useState<DiskPermissionInfo | null>(null);
  const [overview, setOverview] = useState<DiskOverview | null>(null);
  const [sourceScan, setSourceScan] = useState<DiskScan | null>(null);
  const [viewScan, setViewScan] = useState<DiskScan | null>(null);
  const [refreshSnapshot, setRefreshSnapshot] = useState<{ workerId: string; scan: DiskScan }>();
  const refreshReplacement = useRef<{ workerId: string; oldId: string } | undefined>(undefined);
  const previousResult = !viewScan && refreshSnapshot?.workerId === sourceScan?.scanId &&
    (sourceScan?.status !== "complete" || sourceScan.freshness === "partial" || sourceScan.freshness === "updating")
    ? refreshSnapshot?.scan : undefined;
  const showingPrevious = !!previousResult;
  const scan = viewScan ?? previousResult ?? sourceScan;
  const { entries: scanEntries, treemapEntries } = scan ?? {};
  const mapEntries = useMemo(() => {
    if (!scanEntries || !treemapEntries?.length) return scanEntries ?? [];
    const basePaths = new Set(scanEntries.map((entry) => entry.path));
    return [...scanEntries, ...treemapEntries.filter((entry) => !basePaths.has(entry.path))];
  }, [scanEntries, treemapEntries]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [query, setQuery] = useState("");
  const [applicationQuery, setApplicationQuery] = useState("");
  const [applicationSearch, setApplicationSearch] = useState<ApplicationSearchResult | null>(null);
  const [applicationLoading, setApplicationLoading] = useState(false);
  const [applicationError, setApplicationError] = useState("");
  const [applicationRefresh, setApplicationRefresh] = useState(0);
  const applicationRequest = useRef(0);
  const applicationKnown = useRef(new Map<string, DiskApplication>());
  const applicationLastRefresh = useRef(0);
  const [tab, setTab] = useState("folders");
  const [pendingTrashRefresh, setPendingTrashRefresh] = useState<string[]>();
  const [appSelection, setAppSelection] = useState<{ scope: string; paths: Set<string> }>({ scope: "", paths: new Set() });
  const [uninstallPaths, setUninstallPaths] = useState<string[] | null>(null);
  const [lastUninstallAt, setLastUninstallAt] = useState(0);
  const appUninstallRevision = useRef(0);
  const uninstallTrigger = useRef<HTMLButtonElement>(null);
  const appActions = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (page !== "applications") return;
    const request = ++applicationRequest.current;
    let ignore = false;
    setApplicationLoading(true);
    const timer = setTimeout(() => {
      const refresh = applicationRefresh !== applicationLastRefresh.current;
      diskInvoke<ApplicationSearchResult>("search_applications", { query: applicationQuery.trim(), refresh }).then(result => {
        if (ignore || request !== applicationRequest.current) return;
        applicationLastRefresh.current = applicationRefresh;
        for (const app of result.applications) applicationKnown.current.set(app.path, app);
        setApplicationSearch(result);
        setApplicationError("");
      }).catch((error: unknown) => {
        if (!ignore && request === applicationRequest.current) setApplicationError(String(error));
      }).finally(() => {
        if (!ignore && request === applicationRequest.current) setApplicationLoading(false);
      });
    }, applicationQuery.trim() ? 250 : 0);
    return () => { ignore = true; clearTimeout(timer); };
  }, [page, applicationQuery, applicationRefresh]);
  const [notice, setNotice] = useState("");
  const alive = useRef(true);
  const gate = useRef(false);
  const generation = useRef(0);
  const startupScanStarted = useRef(false);
  const scanRef = useRef<DiskScan | null>(null);
  const viewPathRef = useRef<string | undefined>(undefined);
  const viewRequest = useRef(0);
  const viewInFlight = useRef(0);
  const [activeIds, setActiveIds] = useState<string[]>([]);
  const activeScans = useRef(new Map<string, DiskScan>());
  const activeId = sourceScan?.status === "running" && activeIds.includes(sourceScan.scanId)
    ? sourceScan.scanId : activeIds.at(-1);
  const hasProtectedAccess =
    permissionInfo?.status === "granted" || requestedFull;
  async function checkPermissions() {
    const request = ++permissionRequest.current;
    setCheckingPermission(true);
    try {
      const result = await getDiskPermissions(true);
      if (!alive.current || request !== permissionRequest.current) return null;
      permissionRef.current = result;
      setPermissionInfo(result);
      if (result.status === "granted") setPermissionOpen(false);
      return result;
    } catch {
      if (alive.current && request === permissionRequest.current) {
        const result: DiskPermissionInfo = {
          status: "unknown",
          canVerify: false,
          platform: "darwin",
          appPath: permissionRef.current?.appPath ?? "",
          appName: "Cublet",
          isPackaged: true,
          relaunchRecommended: false,
          details: [],
        };
        permissionRef.current = result;
        setPermissionInfo(result);
      }
      return null;
    } finally {
      if (alive.current && request === permissionRequest.current)
        setCheckingPermission(false);
    }
  }
  async function refreshCapacity() {
    try {
      const o = await diskOverview();
      if (alive.current) setOverview(o);
    } catch (e) {
      if (alive.current) setError(String(e));
    }
  }
  function observeWorker(s: DiskScan) {
    const workers = activeScans.current;
    const known = workers.has(s.scanId);
    if (s.status === "running") workers.set(s.scanId, s);
    else workers.delete(s.scanId);
    if (known !== workers.has(s.scanId)) setActiveIds([...workers.keys()]);
  }
  function applyView(view: DiskScan | null) {
    ++viewRequest.current;
    viewInFlight.current = 0;
    viewPathRef.current = view?.viewPath;
    setViewScan(view);
    setSelectedPath(undefined);
    setDirectoryDetailsOpen(false);
  }
  const setCurrentScan = (s: DiskScan) => {
    const previous = scanRef.current;
    if (previous?.scanId === s.scanId && (s.updatedAtMs < previous.updatedAtMs || (previous.status !== "running" && s.status === "running"))) return;
    const changed = previous?.scanId !== s.scanId;
    scanRef.current = s;
    observeWorker(s);
    setSourceScan(s);
    if (changed) applyView(null);
    else if (viewPathRef.current) void updateView(s, viewPathRef.current);
  };
  async function updateView(source: DiskScan, path: string) {
    if (source.status === "running" && viewInFlight.current) return;
    const request = ++viewRequest.current;
    viewInFlight.current = request;
    try {
      const view = await getDiskScanView(source.scanId, path);
      if (alive.current && request === viewRequest.current && scanRef.current?.scanId === source.scanId) setViewScan(view);
    } catch (e) {
      if (alive.current && request === viewRequest.current && scanRef.current?.scanId === source.scanId && !String(e).includes("not-indexed:")) setError(String(e));
    } finally {
      if (viewInFlight.current === request) viewInFlight.current = 0;
    }
  }
  function reportScanError(e: unknown) {
    if (String(e).includes("permission-required:")) {
      setPermissionOpen(true);
      setError(tr("请先设置访问权限，或使用基础扫描范围。", "Set up disk access first, or use a basic scan scope."));
    } else if (String(e).includes("cloud-excluded:")) {
      setError(tr("此目录位于云存储中，未读取云端文件。", "This folder is in cloud storage. Cloud files were not read."));
    } else setError(String(e));
  }
  async function stopActiveScan(id = activeId) {
    if (!id) return;
    let result = await cancelDiskScan(id);
    const deadline = Date.now() + 10000;
    while (result.status === "running" && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 80));
      result = await getDiskScan(id);
    }
    if (result.status === "running") throw new Error(tr("正在停止上次扫描，请稍后重试。", "Previous scan is still stopping. Try again shortly."));
    if (scanRef.current?.scanId === id) setCurrentScan(result);
    else observeWorker(result);
  }
  function rememberSource(source: DiskScan | null) {
    if (!source) return;
    previousSources.current = [
      { scanId: source.scanId, roots: source.roots, allowProtected: sourceProtection.current },
      ...previousSources.current.filter((item) => item.scanId !== source.scanId),
    ].slice(0, 8);
  }
  async function enterDirectory(path: string) {
    const source = scanRef.current;
    if (!source || gate.current || !source.roots.some(root => insideRoot(path, root))) return;
    if (needsScanRefresh(scanFreshness.current, source, path)) {
      if (sourceProtection.current && !hasProtectedAccess) {
        reportScanError("permission-required: disk access changed");
        return;
      }
      await refresh(path, true, sourceProtection.current, false);
      return;
    }
    const sourceRoot = source.roots.includes(path);
    const samePlace = scan?.roots.length === 1 && scan.roots[0] === path;
    const stage = stageRef.current;
    const showDirectoryView = (update: () => void) => {
      const map = !samePlace && stage === stageRef.current ? stage : null;
      return transitionTreemap(map, path, "in", () => {
        update();
        setQuery("");
        setTab("folders");
        setSelectedPath(undefined);
      });
    };
    gate.current = true;
    setBusy(true); setOpeningPath(path); setError(""); setDirectoryDetailsOpen(false);
    ++generation.current;
    let staleInspection = false;
    try {
      if (sourceProtection.current && !hasProtectedAccess) throw new Error("permission-required: disk access changed");
      let displayedSource = previousResult && previousResult.scanId !== source.scanId
        ? await getDiskScan(previousResult.scanId).catch((error: unknown) => {
          if (!String(error).includes("Unknown disk scan ID")) throw error;
          return source;
        }) : source;
      // The preserved map is only a visual placeholder. Never promote an
      // invalidated snapshot to current data when drilling during its refresh.
      if (needsScanRefresh(scanFreshness.current, displayedSource, path)) displayedSource = source;
      let view: DiskScan | undefined;
      try {
        view = await getDiskScanView(displayedSource.scanId, path);
      } catch (e) {
        if (!String(e).includes("not-indexed:") &&
          !(previousResult && String(e).includes("Unknown disk scan ID"))) throw e;
      }
      if (!alive.current) return;
      const fresh = scanRef.current;
      if (displayedSource.scanId !== source.scanId && fresh?.scanId === source.scanId && fresh.status === "complete") {
        const freshView = await getDiskScanView(fresh.scanId, path).catch((error: unknown) => {
          if (!String(error).includes("not-indexed:")) throw error;
          return undefined;
        });
        if (freshView?.viewDetailComplete) {
          displayedSource = fresh;
          view = freshView;
        }
      }
      const liveChildCount = view?.entries.filter(entry => entry.parentPath === path && entry.kind !== "file" && entry.allocatedBytes > 0 && entry.fileCount > 0).length ?? 0;
      if (view?.viewDetailComplete || (view?.status === "running" && liveChildCount >= 2)
          || (sourceRoot && source.status === "running")) {
        await showDirectoryView(() => {
          if (view && displayedSource.scanId !== source.scanId) {
            rememberSource(source);
            refreshReplacement.current = { workerId: source.scanId, oldId: displayedSource.scanId };
            setCurrentScan(displayedSource);
          }
          if (view) applyView(view);
          else if (!samePlace) {
            viewPathRef.current = path;
            void updateView(source, path);
          }
        });
      } else {
        let next: DiskScan | undefined;
        try {
          next = await inspectDiskDirectory(source.scanId, path);
        } catch (e) {
          const competing = [...activeScans.current.keys()].reverse();
          if (!scanBusy(e) || !competing.length) throw e;
          for (const id of competing) {
            await stopActiveScan(id);
            try {
              next = await inspectDiskDirectory(source.scanId, path);
              break;
            } catch (retry) {
              if (!scanBusy(retry) || id === competing.at(-1)) throw retry;
            }
          }
          if (!next) throw e;
        }
        if (!next || !alive.current) return;
        const result = next;
        if (needsScanRefresh(scanFreshness.current, result, path)) staleInspection = true;
        else {
          await showDirectoryView(() => {
            if (result.scanId !== source.scanId) rememberSource(source);
            setCurrentScan(result);
            if (result.scanId === source.scanId && view) applyView(view);
          });
        }
      }
    } catch (e) {
      if (alive.current) reportScanError(e);
    } finally {
      gate.current = false;
      if (alive.current) { setOpeningPath(undefined); setBusy(false); }
    }
    if (staleInspection && alive.current) await refresh(path, true, sourceProtection.current, false);
  }
  async function browse(path?: string) {
    if (gate.current) return;
    stopTreemapTransition();
    const source = scanRef.current;
    if (source && path && source.roots.some(root => insideRoot(path, root))) {
      await enterDirectory(path);
    } else if (source && !path && source.roots.length > 1) {
      if (needsScanRefresh(scanFreshness.current, source)) await refresh(undefined, true);
      else applyView(null);
    } else {
      await refresh(path, false, hasProtectedAccess);
    }
  }
  async function refresh(
    path?: string,
    force = false,
    allowProtected = hasProtectedAccess,
    persistScope = true,
    previousView?: DiskScan,
    mode: DiskScanMode = "auto",
    refreshCached = false,
  ) {
    if (gate.current) return;
    stopTreemapTransition();
    gate.current = true;
    void refreshCapacity();
    setBusy(true);
    setSelectedPath(undefined);
    setError("");
    const round = ++generation.current;
    try {
      // A cached scope can be opened while another scan is running. Ask Rust
      // before cancelling any work; only a real new scan needs that handover.
      let next: DiskScan | undefined;
      if (!force) {
        try {
          next = await startDiskScan(path, false, allowProtected, mode);
          if (needsScanRefresh(scanFreshness.current, next, path) || (refreshCached && next.cacheHit && next.status !== "running")) {
            // Startup may paint the exact permission-compatible cache, but it
            // must also audit the filesystem. Keep both steps under this gate;
            // a cold start or an already-running worker needs no second scan.
            if (refreshCached && alive.current && round === generation.current) {
              previousView = next;
              sourceProtection.current = allowProtected;
              setCurrentScan(next);
            }
            next = undefined;
            force = true;
          }
        } catch (e) {
          if (!scanBusy(e)) throw e;
        }
      }
      if (!next) {
        for (const id of activeScans.current.keys()) await stopActiveScan(id);
      }
      next ??= await startDiskScan(path, force, allowProtected, mode);
      if (force && !next.cacheHit) {
        scanFreshness.current = recordScanRefresh(scanFreshness.current, next);
        try { window.localStorage.setItem(SCAN_FRESHNESS_KEY, JSON.stringify(scanFreshness.current)); }
        catch { /* In-memory invalidation still protects this session. */ }
      }
      if (alive.current && round === generation.current) {
        refreshReplacement.current = undefined;
        if (next.scanId !== scanRef.current?.scanId) rememberSource(scanRef.current);
        const previous = previousView ?? scan;
        const canReuse = force && sourceProtection.current === allowProtected && previous?.status === "complete" &&
          previous.freshness !== "partial" && previous.freshness !== "updating" &&
          previous.roots.length === next.roots.length && previous.roots.every((root, index) => root === next.roots[index]);
        setRefreshSnapshot(canReuse ? { workerId: next.scanId, scan: previous } : undefined);
        sourceProtection.current = allowProtected;
        applyView(null);
        setCurrentScan(next);
        try {
          if (persistScope)
            window.localStorage.setItem(
              "mac-storage-manager.scan-scope.v1",
              path ?? "",
            );
        } catch {
          /* Best-effort preference. */
        }
      }
    } catch (e) {
      if (alive.current) reportScanError(e);
    } finally {
      gate.current = false;
      if (alive.current) setBusy(false);
    }
  }
  useEffect(() => {
    alive.current = true;
    diskOverview()
      .then((o) => {
        if (alive.current) setOverview(o);
      })
      .catch((e) => {
        if (alive.current) setError(String(e));
      });
    void checkPermissions();
    return () => {
      alive.current = false;
    };
  }, []);
  useEffect(() => {
    if (startupScanStarted.current || checkingPermission || !permissionInfo) return;
    startupScanStarted.current = true;
    // Wait for the latest permission result, including a check superseded by
    // focus events, and never replace a scope explicitly chosen during startup.
    if (generation.current !== 0) return;
    let savedScope: string | undefined;
    try {
      savedScope =
        window.localStorage.getItem("mac-storage-manager.scan-scope.v1") ||
        undefined;
    } catch {
      /* Use default roots. */
    }
    const restore = permissionInfo.status === "granted" && permissionChoice === "full";
    void refresh(restore ? savedScope : "/Applications", false, permissionInfo.status === "granted", false, undefined, "auto", true);
  }, [checkingPermission, permissionInfo, permissionChoice]);
  useEffect(() => {
    if (
      checkingPermission ||
      !permissionInfo ||
      initialPermissionDecision.current
    )
      return;
    initialPermissionDecision.current = true;
    if (permissionInfo.status !== "granted" && permissionChoice === null)
      setPermissionOpen(true);
  }, [checkingPermission, permissionInfo, permissionChoice]);
  useEffect(() => {
    const focus = () => {
      void checkPermissions();
    };
    const visible = () => {
      if (document.visibilityState === "visible") void checkPermissions();
    };
    window.addEventListener("focus", focus);
    document.addEventListener("visibilitychange", visible);
    return () => {
      window.removeEventListener("focus", focus);
      document.removeEventListener("visibilitychange", visible);
    };
  }, []);
  useEffect(() => {
    const element = stageRef.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) =>
      setTreemapHeight(Math.max(240, Math.floor(entry.contentRect.height))),
    );
    observer.observe(element);
    setTreemapHeight(Math.max(240, Math.floor(element.clientHeight)));
    return () => observer.disconnect();
  }, [page, explorerLayout, explorerMetric]);
  useEffect(() => {
    if (!activeIds.length) return;
    let stopped = false;
    const timers = new Map<string, ReturnType<typeof setTimeout>>();
    const poll = async (id: string) => {
      const uninstallRevision = appUninstallRevision.current;
      try {
        const next = await getDiskScan(id);
        if (stopped || !activeScans.current.has(id)) return;
        if (uninstallRevision !== appUninstallRevision.current) {
          timers.set(id, setTimeout(() => void poll(id), 0));
          return;
        }
        const replacement = refreshReplacement.current;
        const current = scanRef.current;
        if (current?.scanId === id) {
          setCurrentScan(next);
        } else {
          if (next.status === "complete" && next.freshness !== "partial" && next.freshness !== "updating" && replacement?.workerId === id && replacement.oldId === current?.scanId) {
            const path = viewPathRef.current;
            try {
              const view = path ? await getDiskScanView(id, path) : null;
              if (!stopped && !gate.current && scanRef.current?.scanId === current.scanId && viewPathRef.current === path) {
                if (!path || view?.viewDetailComplete) {
                  refreshReplacement.current = undefined;
                  rememberSource(current);
                  setCurrentScan(next);
                  applyView(view);
                }
              }
            } catch (error) {
              if (!String(error).includes("not-indexed:") && !stopped) reportScanError(error);
            }
          }
          if (!stopped) observeWorker(next);
        }
        if (next.status === "running") {
          timers.set(id, setTimeout(() => void poll(id), next.scannedFiles && (next.sourceScanId === undefined || next.entries.filter(entry => entry.allocatedBytes > 0).length > 3) ? 180 : 40));
        }
      } catch (e) {
        if (!stopped && activeScans.current.has(id)) {
          const worker = activeScans.current.get(id);
          if (worker && String(e).includes("Unknown disk scan ID")) {
            observeWorker({ ...worker, status: "failed" });
          } else {
            timers.set(id, setTimeout(() => void poll(id), 1500));
          }
          setError(String(e));
        }
      }
    };
    for (const id of activeIds) timers.set(id, setTimeout(() => void poll(id), 40));
    return () => { stopped = true; timers.forEach(clearTimeout); };
  }, [activeIds]);
  useEffect(() => {
    if (page === "explorer") void refreshCapacity();
    if (page === "settings") void checkPermissions();
  }, [page]);
  const home = overview?.defaultRoots.find(
    (r) => r.path !== "/Applications",
  )?.path;
  const currentPath = scan?.roots.length === 1 ? scan.roots[0] : undefined;
  const showDirectory = page === "explorer" && explorerLayout === "list" && !!currentPath;
  const rootEntries = (scan?.entries ?? [])
    .filter((e) => scan?.roots.includes(e.parentPath))
    .sort((a, b) => b.allocatedBytes - a.allocatedBytes);
  const visibleEntries = (
    tab === "files"
      ? (scan?.entries ?? []).filter((e) => e.kind === "file")
      : rootEntries.filter((e) => e.kind !== "file")
  )
    .filter((e) =>
      `${e.name} ${e.path}`.toLowerCase().includes(query.toLowerCase()),
    )
    .sort((a, b) => b.allocatedBytes - a.allocatedBytes);
  const applications = applicationSearch?.applications ?? [];
  const appQuery = applicationQuery.trim().toLowerCase();
  const apps = applications
    .filter((app) => `${app.name} ${app.bundleId ?? ""} ${app.path}`.toLowerCase().includes(appQuery))
    .map((app) => ({ ...app, totalBytes: app.bundleBytes + app.supportBytes + app.cacheBytes }))
    .sort((a, b) => (b.sharedData ? b.bundleBytes : b.totalBytes) - (a.sharedData ? a.bundleBytes : a.totalBytes));
  const appSelectionScope = "applications";
  const selectedApps = [...applicationKnown.current.values()].filter(({ path }) =>
    appSelection.scope === appSelectionScope && appSelection.paths.has(path),
  );
  const selectedAppPaths = new Set(selectedApps.map(({ path }) => path));
  const visibleSelectedCount = apps.filter(({ path }) => selectedAppPaths.has(path)).length;
  const allVisibleSelected = apps.length > 0 && visibleSelectedCount === apps.length;
  const atAppSelectionLimit = selectedApps.length >= APP_UNINSTALL_LIMIT;
  const clearVisibleSelection = allVisibleSelected || atAppSelectionLimit;
  function changeAppSelection(paths: string[], checked: boolean) {
    setAppSelection((previous) => {
      const next = new Set(previous.scope === appSelectionScope
        ? [...applicationKnown.current.keys()].filter(path => previous.paths.has(path))
        : []);
      for (const path of paths) {
        if (checked) {
          if (next.size < APP_UNINSTALL_LIMIT) next.add(path);
        }
        else next.delete(path);
      }
      return { scope: appSelectionScope, paths: next };
    });
  }
  function appUninstallFinished(movedPaths: string[], movedApps: string[]) {
    if (!movedPaths.length) return;
    setLastUninstallAt(Date.now());
    if (movedApps.length) {
      const moved = new Set(movedApps);
      for (const path of moved) applicationKnown.current.delete(path);
      ++applicationRequest.current;
      setApplicationLoading(false);
      setApplicationSearch(previous => previous && { ...previous, applications: previous.applications.filter(({ path }) => !moved.has(path)) });
      setAppSelection(previous => ({ scope: appSelectionScope, paths: new Set([...previous.paths].filter(path => !moved.has(path))) }));
      const prune = (snapshot: DiskScan | null) => snapshot && {
        ...snapshot,
        applications: snapshot.applications.filter(({ path }) => !moved.has(path)),
      };
      ++appUninstallRevision.current;
      ++viewRequest.current;
      viewInFlight.current = 0;
      scanRef.current = prune(scanRef.current);
      for (const [id, worker] of activeScans.current) activeScans.current.set(id, prune(worker)!);
      setSourceScan(prune);
      setViewScan(prune);
      setRefreshSnapshot(previous => previous && {
        ...previous,
        scan: prune(previous.scan)!,
      });
      changeAppSelection(movedApps, false);
    }
    void refreshCapacity();
  }
  const selectedEntry = mapEntries.find(
    (entry) => entry.path === selectedPath,
  );
  const running = sourceScan?.status === "running";
  const freshness = sourceScan?.freshness ?? scan?.freshness;
  const needsRetry = !activeId && (freshness === "partial" || freshness === "updating" || sourceScan?.status === "cancelled" || sourceScan?.status === "failed");
  const partial = scan?.status !== "complete" || !scan?.coverageComplete || freshness === "partial" || freshness === "updating";
  async function openFolder(entry: DiskEntry) {
    setPage("explorer");
    setQuery("");
    setTab("folders");
    await enterDirectory(entry.path);
  }
  async function reveal(path: string) {
    try {
      await diskInvoke("reveal_path", { path });
    } catch (e) {
      setError(String(e));
    }
  }
  async function choosePermission(choice: PermissionChoice) {
    setPermissionChoice(choice);
    setRequestedFull(choice === "full");
    try {
      window.localStorage.setItem(PERMISSION_CHOICE_KEY, choice);
    } catch {}
    setPermissionOpen(false);
    setPage("explorer");
    await checkPermissions();
    await refresh(
      choice === "full" ? undefined : "/Applications",
      false,
      choice === "full",
    );
  }
  function handleTrashed(path: string, resolvedPath = path, result?: import("./lib/disk-api").EntryTrashResult) {
    const paths = [...new Set([path, resolvedPath])];
    const at = Date.now();
    scanFreshness.current = recordScanRefresh(scanFreshness.current, { scanId: `trash:${at}`, roots: result?.action === "empty-trash" ? paths : paths.map(item => item.slice(0, item.lastIndexOf("/")) || "/"), startedAtMs: at });
    try { window.localStorage.setItem(SCAN_FRESHNESS_KEY, JSON.stringify(scanFreshness.current)); } catch { /* Keep in-memory invalidation. */ }
    setSelectedPath(undefined);
    if (result?.action === "empty-trash") {
      setNotice(result.remainingCount !== 0 || result.failures?.length ? tr(`已永久删除 ${result.removedCount ?? 0} 项，剩余 ${result.remainingCount ?? "未知"} 项。${result.failures?.[0]?.message ?? "新增项目已保留，可重新检查。"}`, `Permanently deleted ${result.removedCount ?? 0} items; ${result.remainingCount ?? "unknown"} remain. ${result.failures?.[0]?.message ?? "New items were kept. Check again to retry."}`) : tr("已清空当前用户的废纸篓。", "This user’s Trash is empty."));
    } else setNotice(tr("已移到废纸篓。需要之后清空废纸篓，才会真正释放磁盘空间；清空前可在 Finder 中恢复。", "Moved to Trash. Empty Trash afterwards to free disk space; restore through Finder before emptying it."));
    setPendingTrashRefresh(previous => [...(previous ?? []), ...(result?.action === "empty-trash" ? [] : paths)]);
  }
  useEffect(() => {
    if (!pendingTrashRefresh || busy || gate.current) return;
    // Navigation may have finished while native Trash was working. Refresh the
    // latest displayed scope, not the scope captured when the move started.
    let destination = currentPath;
    for (const path of [...pendingTrashRefresh].sort((a, b) => a.length - b.length)) {
      if (destination && insideRoot(destination, path)) destination = path.slice(0, path.lastIndexOf("/")) || "/";
    }
    setPendingTrashRefresh(undefined);
    void refresh(destination, true);
  }, [pendingTrashRefresh, busy, currentPath]);
  const mapDetails = selectedEntry ? <SpaceSelection
    name={selectedEntry.name} path={selectedEntry.path} locale={locale} side={panelSide}
    metrics={[{ label: tr("占用", "Allocated"), value: formatBytes(selectedEntry.allocatedBytes) },
      { label: tr("文件大小", "Logical size"), value: formatBytes(selectedEntry.logicalBytes) },
      { label: tr("文件数", "Files"), value: selectedEntry.fileCount.toLocaleString() }]}
    partial={!selectedEntry.complete} onClose={() => setSelectedPath(undefined)}
    onOpen={selectedEntry.kind !== "file" ? () => void openFolder(selectedEntry) : undefined}
    onReveal={() => void reveal(selectedEntry.path)}
    onPermissionRequired={() => { setPermissionOpen(true); void checkPermissions(); }}
    onTrashed={handleTrashed}
  /> : null;
  const baseRoots = [home, "/Applications", "/Library", "/System/Volumes/Data"];
  const parentPath = currentPath && currentPath !== "/" && !baseRoots.includes(currentPath)
    ? currentPath.slice(0, currentPath.lastIndexOf("/")) || "/"
    : undefined;
  const folderRoot = (path: string) => baseRoots.find((root): root is string => !!root && insideRoot(path, root));
  const parentScope = parentPath && folderRoot(parentPath);
  const canGoBack = !!parentScope || (!!currentPath && (currentPath === home || currentPath === "/Applications") &&
    (sourceScan?.roots.length !== 1 || sourceProtection.current));
  const openAncestor = async (path?: string, openUnscanned = false) => {
    const current = scanRef.current;
    if (!current || gate.current) return;
    gate.current = true;
    setBusy(true);
    setError("");
    setNotice("");
    ++generation.current;
    const allowProtected = sourceProtection.current;
    const stage = stageRef.current;
    let refreshRequired = false;
    let previousView: DiskScan | undefined;
    const containsAncestor = (roots: string[]) => path
      ? roots.some((root) => insideRoot(path, root))
      : roots.length > 1;
    const restore = async (source: DiskScan | null) => {
      if (!source || !containsAncestor(source.roots)) return false;
      const stale = needsScanRefresh(scanFreshness.current, source, path);
      refreshRequired ||= stale;
      let view: DiskScan | null = null;
      if (path && (source.roots.length !== 1 || source.roots[0] !== path)) {
        try {
          view = await getDiskScanView(source.scanId, path);
        } catch (e) {
          if (String(e).includes("Unknown disk scan ID") || String(e).includes("not-indexed:")) return false;
          throw e;
        }
      }
      if (stale) {
        previousView ??= view ?? source;
        return false;
      }
      if (!alive.current) return true;
      const update = () => {
        if (source.scanId !== current.scanId) rememberSource(current);
        setCurrentScan(source);
        applyView(view);
        setQuery("");
      };
      if (currentPath && path === parentPath && stage === stageRef.current) {
        await transitionTreemap(stage, currentPath, "out", update);
      } else {
        update();
      }
      return true;
    };
    try {
      if (allowProtected && !hasProtectedAccess) throw new Error("permission-required: disk access changed");
      const replacement = refreshReplacement.current;
      if (replacement?.oldId === current.scanId) {
        const fresh = await getDiskScan(replacement.workerId).catch(() => null);
        if (fresh?.status === "complete" && await restore(fresh)) {
          refreshReplacement.current = undefined;
          return;
        }
      }
      if (await restore(current)) return;
      const evictedScopes = new Set<string | undefined>();
      for (const source of previousSources.current) {
        if (source.scanId === current.scanId || source.allowProtected !== allowProtected || !containsAncestor(source.roots)) continue;
        const snapshot = await getDiskScan(source.scanId).catch((e: unknown) => {
          if (!String(e).includes("Unknown disk scan ID")) throw e;
          evictedScopes.add(source.roots.length === 1 ? source.roots[0] : undefined);
          return null;
        });
        if (await restore(snapshot)) return;
      }
      const scopes = new Set([path, ...evictedScopes, path ? folderRoot(path) : undefined, undefined]);
      for (const scope of scopes) {
        if (await restore(await getCachedDiskScan(scope, allowProtected, current.scanId))) return;
      }
      if (!openUnscanned && !refreshRequired) {
        setNotice(tr("该目录还没有扫描结果。点击顶部路径即可打开。", "No scan results for this folder. Open it from the path above."));
      }
    } catch (e) {
      if (alive.current) reportScanError(e);
      return;
    } finally {
      gate.current = false;
      if (alive.current) setBusy(false);
    }
    if ((refreshRequired || openUnscanned) && alive.current) {
      await refresh(path, refreshRequired, allowProtected, false, previousView);
    }
  };
  const back = () => { if (canGoBack) void openAncestor(parentPath); };
  useEffect(() => {
    if (page === "explorer") currentLocationRef.current?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [page, currentPath]);
  useEffect(() => {
    if (busy || !focusLocation.current) return;
    focusLocation.current = false;
    currentLocationRef.current?.focus();
  }, [busy, currentPath]);
  useEffect(() => {
    if (page !== "explorer" || explorerMetric !== "allocation" || directoryDetailsOpen || !canGoBack) return;
    const handler = (event: KeyboardEvent) => {
      if (event.defaultPrevented || (event.target instanceof HTMLElement &&
        event.target.closest('input, textarea, [contenteditable="true"], [role="dialog"], [role="listbox"]'))) return;
      if ((event.metaKey && event.key === "[") || (event.altKey && event.key === "ArrowLeft")) {
        event.preventDefault();
        void back();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  });
  const homeAndApps = tr("用户目录与应用", "Home + Apps");
  const locationRoot = currentPath && (folderRoot(currentPath) || sourceScan?.roots.find(root => insideRoot(currentPath, root)) || currentPath);
  const breadcrumbs = currentPath && locationRoot ? [
    { path: locationRoot, name: locationRoot === home ? tr("用户目录", "Home")
      : locationRoot === "/Applications" ? tr("应用程序", "Applications")
        : locationRoot === "/Library" ? tr("系统资源库", "System Library")
          : locationRoot === "/System/Volumes/Data" ? tr("数据卷", "Data volume") : locationRoot },
    ...currentPath.slice(locationRoot.length).split("/").filter(Boolean).map((name, index, parts) => ({
      path: `${locationRoot === "/" ? "" : locationRoot}/${parts.slice(0, index + 1).join("/")}`,
      name,
    })),
  ] : [];
  const scopeControl = (
    <NativeSelect
      value={page === "explorer" ? "scan-scope" : currentPath ?? ""}
      disabled={busy}
      onValueChange={(value) => void browse(value || undefined)}
      aria-label={tr("扫描范围", "Scan scope")}
    >
      {page === "explorer" && <option value="scan-scope" disabled>{tr("扫描范围", "Scan scope")}</option>}
      <option value="">{homeAndApps}</option>
      {home && (
        <>
          <option value={home}>{tr("用户目录", "Home")}</option>
          <option value={`${home}/Applications`}>{tr("用户应用程序", "User Applications")}</option>
          <option value={`${home}/Downloads`}>
            {tr("下载目录", "Downloads")}
          </option>
          <option value={`${home}/.codex`}>Codex</option>
          <option value={`${home}/.claude`}>Claude Code</option>
        </>
      )}
      <option value="/Applications">{tr("应用程序", "Applications")}</option>
      <option value="/Library">{tr("系统资源库", "System Library")}</option>
      <option value="/System/Volumes/Data">
        {tr("整个数据卷", "Data volume")}
      </option>
      {page === "applications" && currentPath &&
        ![
          ...baseRoots,
          `${home}/Applications`,
          `${home}/Downloads`,
          `${home}/.codex`,
          `${home}/.claude`,
        ].includes(currentPath) && (
          <option value={currentPath}>{folderName(currentPath)}</option>
        )}
    </NativeSelect>
  );
  const rescanControls = (
    <>
      {activeId && (
        <IconButton
          label={tr("停止扫描", "Stop scan")}
          disabled={busy}
          onClick={() => void stopActiveScan().catch(reportScanError)}
        >
          <Square size={13} />
        </IconButton>
      )}
      <IconButton
        variant="ghost"
        size="icon-sm"
        disabled={busy}
        label={tr("重新分析", "Rescan")}
        onClick={() => void refresh(currentPath, true, hasProtectedAccess, false)}
      >
        <RefreshCw size={15} className={busy ? "spin" : ""} />
      </IconButton>
    </>
  );
  const scannedItemCount = page === "applications"
    ? applicationQuery.trim()
      ? tr(`显示 ${apps.length} / ${applications.length} 个应用`, `${apps.length} of ${applications.length} apps`)
      : tr(`${applications.length} 个应用`, `${applications.length} apps`)
    : scan && `${formatBytes(scan.allocatedBytes)} · ${scan.scannedFiles.toLocaleString()} ${tr("个文件", "files")}`;
  const snapshotBeforeUninstall = !!scan && lastUninstallAt > 0 && (scan.startedAtMs <= lastUninstallAt || scan.status !== "complete");
  const statusFooter = (
    page === "applications" ? <footer className="analysis-status" role="status">
      <span>{applicationLoading && <LoaderCircle size={12} className="spin" />}{applicationLoading ? tr("正在查找应用…", "Finding applications…") : scannedItemCount}
        {applicationSearch?.partial && <span>{tr("部分结果", "Partial results")}</span>}</span>
      {applicationError && <span className="app-uninstall-stale">{tr("刷新失败，保留上次结果", "Refresh failed · previous results")}</span>}
    </footer> :
    <footer className="analysis-status" role="status">
      <span
        title={
          scan?.cacheHit
            ? tr(
                "来自上次扫描，重新分析可刷新",
                "Previous scan. Rescan to refresh.",
              )
            : undefined
        }
      >
        {(running || openingPath || activeId) && <LoaderCircle size={12} className="spin" />}
        {openingPath
          ? tr(`正在打开 ${folderName(openingPath)}…`, `Opening ${folderName(openingPath)}…`)
          : showingPrevious
          ? running
            ? tr(`显示上次结果 · 正在更新 · 已扫描 ${(sourceScan?.scannedFiles ?? 0).toLocaleString()} 项 · ${Math.floor((sourceScan?.elapsedMs ?? 0) / 1000)}s`, `Previous results · Updating · ${(sourceScan?.scannedFiles ?? 0).toLocaleString()} scanned · ${Math.floor((sourceScan?.elapsedMs ?? 0) / 1000)}s`)
            : sourceScan?.status === "cancelled"
            ? tr("显示上次结果 · 更新已停止", "Previous results · Update stopped")
            : sourceScan?.status === "failed"
            ? tr("显示上次结果 · 更新失败", "Previous results · Update failed")
            : tr("显示上次结果 · 更新未完成", "Previous results · Update incomplete")
          : showDirectory && !running
          ? tr("目录明细 · 文件夹占用来自扫描结果", "Directory listing · folder sizes from scan results")
          : scan
          ? `${scannedItemCount}${running ? ` · ${tr("后台统计占用", "Measuring disk usage in background")}` : scan.status === "cancelled" ? ` · ${scan.cacheHit ? tr("未完成缓存", "Partial cache") : tr("已停止", "Stopped")}` : scan.status === "failed" ? ` · ${tr("扫描失败", "Scan failed")}` : scan.cacheHit ? ` · ${tr("缓存", "Cached")}` : ""}`
          : tr("准备扫描…", "Preparing scan…")}
        {page === "explorer" && !showDirectory && scan?.viewPath && !openingPath && <span>{tr("索引视图", "Index view")}</span>}
        {activeId && !running && !openingPath && <span>{tr("后台分析中", "Analyzing in background")}</span>}
      </span>
      {overview && (
        <span
          className="capacity-status"
          title={tr(
            `总量 ${formatBytes(overview.capacity.totalBytes)}，已用 ${formatBytes(overview.capacity.usedBytes)}，可用 ${formatBytes(overview.capacity.availableBytes)}`,
            `Total ${formatBytes(overview.capacity.totalBytes)}, used ${formatBytes(overview.capacity.usedBytes)}, available ${formatBytes(overview.capacity.availableBytes)}`,
          )}
        >
          {tr("可用", "Free")} {formatBytes(overview.capacity.availableBytes)}
          <span className="capacity-used">
            {" "}
            · {tr("已用", "Used")} {formatBytes(overview.capacity.usedBytes)} /{" "}
            {formatBytes(overview.capacity.totalBytes)}
          </span>
        </span>
      )}
      <span className="analysis-status-actions">
        {(scan?.usageStale || snapshotBeforeUninstall) && <span className="app-uninstall-stale" title={tr("已移动项目，但当前扫描的占用统计尚未更新。重新分析可刷新。", "Items moved. Rescan to update usage figures.")}>{tr("占用统计待更新", "Usage out of date")}</span>}
        {(partial || showingPrevious) && scan ? (
          <span>{freshness === "partial" ? tr("部分结果", "Partial") : running || freshness === "updating" ? tr("正在更新", "Updating") : showingPrevious ? tr("上次结果", "Previous scan") : tr("部分结果", "Partial")}</span>
        ) : null}
        {needsRetry && !(page === "explorer" && explorerMetric === "growth") && <button disabled={busy} onClick={() => void refresh(currentPath, true, hasProtectedAccess, false)}>{tr("重试扫描", "Retry scan")}</button>}
        <button
          aria-label={tr("扫描详情", "Scan details")}
          title={tr("扫描详情", "Scan details")}
          onClick={() => setScanDetailsOpen(true)}
        >
          <CircleHelp size={13} />
          {!!scan?.issueCount && <span>{tr(`${scan.issueCount} 条说明`, `${scan.issueCount} notes`)}</span>}
        </button>
      </span>
    </footer>
  );
  const map = (
    <DiskTreemap
      entries={mapEntries}
      roots={scan?.roots ?? []}
      onOpenDirectory={(entry) => void openFolder(entry)}
      onRevealFile={(entry) => void reveal(entry.path)}
      onInspectDirectory={(path) => void enterDirectory(path)}
      onSelect={(entry, side) => { setPanelSide(side ?? "right"); setSelectedPath(entry.path); }}
      locale={locale}
      isScanning={running}
      selectedPath={selectedPath}
      onClearSelection={() => setSelectedPath(undefined)}
      query={page === "explorer" ? query : ""}
      height={treemapHeight}
      minimal
    />
  );
  function navigate(id: DesktopPage) {
    if (id === "codex" && !hasProtectedAccess) {
      setPermissionOpen(true);
      return;
    }
    setPage(id);
    setQuery("");
  }
  const showCodex = page === "codex" && hasProtectedAccess;
  const workspaceLayout = showCodex ? "embedded" : page === "explorer" || page === "applications" ? "canvas" : "scroll";

  return (
    <TooltipProvider delay={350}>
      <DesktopShell
        page={page}
        labels={labels}
        navigationLabel={tr("主导航", "Main navigation")}
        layout={workspaceLayout}
        onNavigate={navigate}
      >
        {showCodex ? (
          <Suspense fallback={
            <div className="loading-state" role="status">
              <LoaderCircle className="spin" /><span>{tr("正在加载 Codex 工具…", "Loading Codex tools…")}</span>
            </div>
          }>
            <CodexTools embedded onStorageChanged={() => void refreshCapacity()} />
          </Suspense>
        ) : (
          <>
            {error && (
              <div className="error-banner" role="alert">
                <span>{error}</span>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  aria-label={tr("关闭错误", "Dismiss error")}
                  onClick={() => setError("")}
                >
                  <X size={14} />
                </Button>
              </div>
            )}
            {notice && (
              <div className="mac-notice" role="status">
                {notice}
                <button
                  onClick={() => setNotice("")}
                  aria-label={tr("关闭提示", "Dismiss")}
                >
                  ×
                </button>
              </div>
            )}
            {(page === "explorer" || page === "applications") && (
              <>
                <div className="analysis-toolbar" role="group" aria-label={tr("分析工具", "Analysis tools")}>
                  {page === "explorer" && <div className="scope-control">
                    {scopeControl}
                  </div>}
                  <WorkspaceSearch
                    value={page === "applications" ? applicationQuery : query}
                    onChange={page === "applications" ? setApplicationQuery : setQuery}
                    label={page === "applications" ? tr("搜索应用名称或路径", "Search applications by name or path") : showDirectory && explorerMetric === "allocation"
                      ? tr("搜索当前页名称或路径", "Search this page by name or path")
                      : tr("搜索名称或路径", "Search name or path")}
                    placeholder={page === "applications" ? tr("搜索应用", "Search applications") : showDirectory && explorerMetric === "allocation"
                      ? tr("搜索当前页", "Search this page")
                      : tr("搜索", "Search")}
                    clearLabel={tr("清空搜索", "Clear search")}
                  />
                  <div className="analysis-toolbar-spacer" />
                  {page === "explorer" && (
                    <AnimatedSegmentedControl value={explorerMetric} onValueChange={setExplorerMetric} ariaLabel={tr("分析模式", "Analysis mode")}
                      items={[{ value: "allocation", label: tr("当前占用", "Current usage") }, { value: "growth", label: tr("新增占用", "Growth") }]} />
                  )}
                  {page === "explorer" && explorerMetric === "allocation" && (
                    <AnimatedSegmentedControl value={explorerLayout} onValueChange={setExplorerLayout} ariaLabel={tr("视图", "View")}
                      items={[{ value: "map", label: tr("树图", "Map"), icon: <Layers size={14} /> }, { value: "list", label: tr("列表", "List") }]} />
                  )}
                  {page === "explorer" && explorerMetric === "allocation" && explorerLayout === "list" && !currentPath && (
                    <div className="entry-type">
                      <NativeSelect
                        value={tab}
                        onValueChange={setTab}
                        aria-label={tr("查看类型", "Entry type")}
                      >
                        <option value="folders">{tr("目录", "Folders")}</option>
                        <option value="files">
                          {tr("大文件快照", "Recorded large files")}
                        </option>
                      </NativeSelect>
                    </div>
                  )}
                  <div className="analysis-toolbar-actions">{page === "applications" ? <IconButton variant="ghost" size="icon-sm"
                    disabled={applicationLoading} label={tr("刷新应用清单", "Refresh applications")} onClick={() => setApplicationRefresh(value => value + 1)}>
                    <RefreshCw size={15} className={applicationLoading ? "spin" : ""} />
                  </IconButton> : rescanControls}</div>
                </div>
                {page === "explorer" && explorerMetric === "allocation" && (
                  <div className="directory-location">
                    <nav aria-label={tr("当前路径", "Current path")} title={currentPath}>
                      <ol>
                        {!currentPath ? <li><span ref={currentLocationRef} tabIndex={-1} aria-current="location">{homeAndApps}</span></li> : (
                          <>
                            {(locationRoot === home || locationRoot === "/Applications") && (
                              <li><button type="button" disabled={busy} onClick={(event) => {
                                focusLocation.current = event.detail === 0;
                                void openAncestor(undefined, true);
                              }}>{homeAndApps}</button></li>
                            )}
                            {breadcrumbs.map(({ path, name }) => (
                              <li key={path}>
                                {path === currentPath ? <span ref={currentLocationRef} tabIndex={-1} aria-current="location" title={path}>{name}</span>
                                  : <button type="button" disabled={busy} title={path} onClick={(event) => {
                                    focusLocation.current = event.detail === 0;
                                    void openAncestor(path, true);
                                  }}>{name}</button>}
                              </li>
                            ))}
                          </>
                        )}
                      </ol>
                    </nav>
                    {currentPath && explorerLayout === "map" && (
                      <Button variant="ghost" size="sm" onClick={() => {
                        if (sourceProtection.current && !hasProtectedAccess) {
                          reportScanError("permission-required: disk access changed");
                        } else setDirectoryDetailsOpen(true);
                      }}>
                        <FolderOpen aria-hidden="true" />{tr("目录明细", "Directory details")}
                      </Button>
                    )}
                  </div>
                )}
                <motion.div className="analysis-body" key={`${page}-${explorerMetric}-${explorerLayout}`}
                  initial={reducedMotion ? false : { opacity: .7, y: 4 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: .16 }}>
                {page === "applications" ? (
                  <>
                  {(applicationError || !!applicationSearch?.notices.length) && <div className="application-notices">
                    {applicationError && <div role="alert"><span>{tr("无法更新应用清单。", "Couldn’t update applications.")} {applicationError}</span>
                      <Button variant="outline" size="sm" disabled={applicationLoading} onClick={() => setApplicationRefresh(value => value + 1)}>{tr("重试", "Retry")}</Button></div>}
                    {applicationSearch?.notices.map((message, index) => <p key={index}>{message}</p>)}
                  </div>}
                  <div className="app-uninstall-actions" ref={appActions} tabIndex={-1}>
                    <span className="app-uninstall-selection" role="status">
                      {selectedApps.length ? tr(`已选 ${selectedApps.length} 个应用`, `${selectedApps.length} selected`) : tr("选择要卸载的应用", "Select apps to uninstall")}
                      {atAppSelectionLimit && tr(" · 已达单次上限", " · Batch limit reached")}
                    </span>
                    {selectedApps.length > 0 && (
                      <Button variant="ghost" onClick={() => setAppSelection({ scope: appSelectionScope, paths: new Set() })}>
                        {tr("清空选择", "Clear selection")}
                      </Button>
                    )}
                    <Button ref={uninstallTrigger} variant="outline" disabled={!selectedApps.length} onClick={() => setUninstallPaths(selectedApps.map(({ path }) => path))}>
                      <Trash2 aria-hidden="true" />
                      {tr("预览卸载", "Review uninstall")}
                    </Button>
                  </div>
                  <div className="application-list disk-table apps-table" role="table" aria-label={labels.applications}>
                    <div className="disk-table-head" role="row">
                      <div className="app-uninstall-checkbox-cell" role="columnheader">
                        <Checkbox
                          checked={allVisibleSelected}
                          indeterminate={visibleSelectedCount > 0 && !allVisibleSelected}
                          disabled={!apps.length || (atAppSelectionLimit && visibleSelectedCount === 0)}
                          aria-label={clearVisibleSelection ? tr("取消选择当前结果中的应用", "Deselect apps in these results") : tr("选择当前结果中的应用", "Select apps in these results")}
                          onCheckedChange={() => changeAppSelection(apps.map(({ path }) => path), !clearVisibleSelection)}
                        />
                      </div>
                      <span role="columnheader">{tr("应用", "Application")}</span>
                      <span role="columnheader">{tr("本体", "Bundle")}</span>
                      <span role="columnheader">{tr("数据", "Data")}</span>
                      <span role="columnheader">{tr("缓存", "Cache")}</span>
                      <span role="columnheader"
                        title={tr(
                          "仅包含已匹配的数据",
                          "Includes matched data only",
                        )}
                      >
                        {tr("已归属合计", "Attributed")}
                      </span>
                      <span role="columnheader" aria-label={tr("操作", "Actions")} />
                    </div>
                    {apps.map((app) => {
                      const checked = selectedAppPaths.has(app.path);
                      return (
                        <div className="disk-table-row" role="row" key={app.path} data-uninstall-selected={checked || undefined}>
                          <div className="app-uninstall-checkbox-cell" role="cell">
                            <Checkbox
                              checked={checked}
                              disabled={atAppSelectionLimit && !checked}
                              aria-label={`${tr("选择", "Select")} ${app.name} · ${app.path}`}
                              onCheckedChange={(next) => changeAppSelection([app.path], next)}
                            />
                          </div>
                          <div className="disk-name" role="cell">
                            <AppIcon path={app.path} name={app.name} />
                            <span>
                              <strong>{app.name}{app.version && <span className="application-version">{app.version}</span>}</strong>
                              <small title={app.path}>{app.path}</small>
                            </span>
                          </div>
                          <span role="cell">{app.bundleBytesKnown === false ? <span title={tr("预览时测量", "Measured in preview")}>—</span> : formatBytes(app.bundleBytes)}</span>
                          <span role="cell">{app.supportBytes > 0 ? formatBytes(app.supportBytes) : <span title={tr("预览时查找关联文件", "Related files checked in preview")}>—</span>}</span>
                          <span role="cell">{app.cacheBytes > 0 ? formatBytes(app.cacheBytes) : <span title={tr("预览时查找关联缓存", "Related caches checked in preview")}>—</span>}</span>
                          <strong className="app-total" role="cell">
                            {app.bundleBytesKnown === false && app.totalBytes === 0 ? "—" : formatBytes(app.totalBytes)}
                            {app.sharedData && <small title={tr("扫描发现其他副本关联到相同的数据或缓存", "Other copies found by the scan match the same data or cache")}>
                              {tr("含共享数据", "Includes shared data")}
                            </small>}
                          </strong>
                          <div role="cell">
                            <IconButton
                              label={`${tr("在 Finder 显示", "Show in Finder")} · ${app.name}`}
                              title={app.path}
                              onClick={() => void reveal(app.path)}
                            >
                              <ArrowUpRight size={14} />
                            </IconButton>
                          </div>
                        </div>
                      );
                    })}
                    {!apps.length && (
                      <div role="row">
                        <div className="disk-empty" role="cell" aria-colspan={7}>
                          <span role="status">
                            {applicationLoading
                              ? tr("正在查找应用…", "Finding applications…")
                              : applicationError ? tr("应用清单暂不可用，可重试。", "Applications unavailable. Retry to load them.")
                              : applicationQuery.trim()
                                ? tr("没有匹配的应用", "No matching applications")
                                : tr("没有找到应用", "No applications found")}
                          </span>
                        </div>
                      </div>
                    )}
                  </div>
                  </>
                ) : explorerMetric === "growth" ? (
                  <Suspense fallback={<div className="loading-state" role="status"><LoaderCircle className="spin" /><span>{tr("正在读取扫描对比…", "Loading scan comparison…")}</span></div>}>
                    <ScanComparison onTrashed={handleTrashed} onPermissionRequired={() => { setPermissionOpen(true); void checkPermissions(); }} scan={sourceScan ?? scan} preview={scan} currentPath={currentPath} locale={locale} height={treemapHeight} query={query}
                      onRetryScan={() => {
                        if (!sourceScan || busy || activeId || openingPath || checkingPermission) return;
                        if (sourceProtection.current && !hasProtectedAccess) {
                          setPermissionOpen(true); void checkPermissions(); return;
                        }
                        // Growth compares the source scope even when its historical map is drilled.
                        const retryPath = sourceScan.roots.length === 1 ? sourceScan.roots[0] : undefined;
                        void refresh(retryPath, true, sourceProtection.current, false);
                      }}
                      retryScanDisabled={busy || !!activeId || !!openingPath || checkingPermission}
                      retryScanDisabledReason={activeId ? tr("请等待当前扫描完成，或先停止扫描。", "Wait for the active scan to finish, or stop it first.") : busy || openingPath ? tr("正在切换扫描范围。", "The scan scope is changing.") : checkingPermission ? tr("正在检查访问权限。", "Checking disk access.") : sourceProtection.current && !hasProtectedAccess ? tr("请先恢复此范围的访问权限。", "Restore disk access to retry this scope.") : undefined}
                      onBrowseCurrent={(path) => { setExplorerMetric("allocation"); if (path) { setQuery(""); setTab("folders"); void enterDirectory(path); } }}
                      onReveal={(path) => void reveal(path)} />
                  </Suspense>
                ) : showDirectory && currentPath && sourceScan ? (
                  <DirectoryContents source={sourceScan} summaries={scan?.entries} path={currentPath} query={query} locale={locale}
                    onOpen={(path) => void enterDirectory(path)}
                    onReveal={(path) => void reveal(path)} />
                ) : explorerLayout === "map" ? (
                  <div className="explorer-stage" ref={setStage}>
                    <div
                      className={`space-map-layout ${selectedEntry ? "has-selection" : ""}`}
                    >
                      {map}
                      {mapDetails}
                    </div>
                  </div>
                ) : (
                  <div className="file-list disk-table">
                    <div className="disk-table-head">
                      <span>{tr("名称 / 路径", "Name / path")}</span>
                      <span>{tr("文件数", "Files")}</span>
                      <span>{tr("占用", "Allocated")}</span>
                      <span />
                    </div>
                    {visibleEntries.map((entry, index) => (
                      <div className="disk-table-row" key={entry.path}>
                        <button
                          className="disk-name"
                          onClick={() =>
                            entry.kind === "file"
                              ? void reveal(entry.path)
                              : void openFolder(entry)
                          }
                        >
                          {entry.kind === "file" ? (
                            <File size={16} />
                          ) : (
                            <Folder
                              size={16}
                              style={{ color: palette[index % palette.length] }}
                            />
                          )}
                          <span>
                            <strong>{entry.name}</strong>
                            <small title={entry.path}>{entry.path}</small>
                          </span>
                        </button>
                        <span>{entry.fileCount.toLocaleString()}</span>
                        <span
                          title={
                            !entry.complete
                              ? tr("部分统计", "Partial")
                              : undefined
                          }
                        >
                          {formatBytes(entry.allocatedBytes)}
                        </span>
                        <span className="row-actions">
                          <Button
                            variant="ghost"
                            size="icon-sm"
                            onClick={() => void reveal(entry.path)}
                            aria-label={`${tr("在 Finder 显示", "Reveal")} ${entry.name}`}
                          >
                            <ArrowUpRight size={13} />
                          </Button>
                          <TrashAction path={entry.path} name={entry.name} locale={locale} iconOnly onTrashed={handleTrashed} onPermissionRequired={() => { setPermissionOpen(true); void checkPermissions(); }} />
                        </span>
                      </div>
                    ))}
                    {!visibleEntries.length && (
                      <div className="disk-empty" role="status">
                        {running
                          ? tr("正在扫描…", "Scanning…")
                          : query.trim()
                            ? tr("没有匹配的项目", "No matching entries")
                            : tr("当前范围没有可显示的项目", "No entries in this scope")}
                      </div>
                    )}
                  </div>
                )}
                </motion.div>
                {statusFooter}
              </>
            )}
            {page === "ai" && <Suspense fallback={<div className="disk-empty" role="status">{tr("正在加载…", "Loading…")}</div>}><AIConnection locale={locale} /></Suspense>}
            {page === "settings" && (
              <SettingsPanel
                info={permissionInfo}
                checking={checkingPermission}
                scanActive={!!activeId}
                onCheck={() => void checkPermissions()}
                onSetup={() => setPermissionOpen(true)}
              />
            )}
          </>
        )}
      </DesktopShell>
      <PermissionOnboarding
        open={permissionOpen}
        onClose={() => setPermissionOpen(false)}
        onChoose={(choice) => void choosePermission(choice)}
        info={permissionInfo}
        canClose={
          permissionChoice !== null || permissionInfo?.status === "granted"
        }
        busy={busy}
      />
      {uninstallPaths && (
        <AppUninstallDialog
          paths={uninstallPaths}
          locale={locale}
          onClose={() => setUninstallPaths(null)}
          onMoved={appUninstallFinished}
          returnFocus={() => uninstallTrigger.current?.disabled ? appActions.current : uninstallTrigger.current}
        />
      )}
      <Dialog open={directoryDetailsOpen && !!currentPath && page === "explorer"} onOpenChange={setDirectoryDetailsOpen}>
        <DialogContent className="treemap-directory-dialog" closeLabel={tr("关闭", "Close")}>
          <DialogHeader><DialogTitle>{currentPath ? folderName(currentPath) : tr("目录明细", "Directory details")}</DialogTitle></DialogHeader>
          {currentPath && sourceScan && <DirectoryContents source={sourceScan} summaries={scan?.entries} path={currentPath} query="" locale={locale}
            onOpen={(path) => void enterDirectory(path)} onReveal={(path) => void reveal(path)} />}
        </DialogContent>
      </Dialog>
      <Dialog open={scanDetailsOpen} onOpenChange={setScanDetailsOpen}>
        <DialogContent closeLabel={tr("关闭", "Close")}>
          <DialogHeader>
            <DialogTitle>{tr("扫描详情", "Scan details")}</DialogTitle>
            <DialogDescription>{currentPath ?? scan?.roots.join(" + ")}</DialogDescription>
          </DialogHeader>
          <DialogBody>
          {scan && (
            <p>
              {tr("扫描时间：", "Scanned: ")}
              {new Date(scan.updatedAtMs).toLocaleString(
                locale === "zh" ? "zh-CN" : "en-US",
              )}
              {scan.cacheHit &&
                ` · ${tr("缓存结果，重新分析可刷新", "Cached result. Rescan to refresh.")}`}
            </p>
          )}
          {sourceScan?.scanMethod && <p>{tr("更新方式：", "Scan method: ")}{sourceScan.scanMethod === "incremental" ? tr("增量更新", "Incremental update") : sourceScan.scanMethod === "validated-reuse" ? tr("核查后复用索引", "Metadata audit + index reuse") : sourceScan.scanMethod === "cached" ? tr("缓存结果", "Cached result") : tr("完整扫描", "Complete scan")}</p>}
          {(freshness === "partial" || freshness === "updating") && <p>{running
            ? tr("正在更新结果。完成前继续显示上次可用数据，可随时停止扫描。", "Updating results. Previous available data stays visible; you can stop the scan at any time.")
            : tr("本次更新尚未完成，显示的数据未全部验证。重试扫描可继续更新。", "This update is incomplete and its results have not all been verified. Retry the scan to update them.")}</p>}
          {sourceScan?.fallbackReason && <p>{tr("完整扫描原因：", "Complete scan reason: ")}{sourceScan.fallbackReason}</p>}
          {scan?.viewPath && (
            <p>
              {tr(
                "已使用目录索引。进入缺少明细的目录时，只分析该目录。",
                "Using the directory index. Opening a folder with missing details analyzes that folder.",
              )}
              <br />
              {tr("来源范围：", "Source scope: ")}
              {scan.sourceRoots?.join(" + ")}
            </p>
          )}
          {scan?.viewPath && !scan.viewDetailComplete && (
            <p>
              {tr(
                "此目录仍有部分明细未缓存，树图显示已统计的占用。",
                "Some details are not cached. The treemap shows measured usage.",
              )}
            </p>
          )}
          <p>
            {tr(
              "大文件快照仅包含扫描中保留的较大文件，并非所有文件清单。缓存为上次扫描结果，不会自动当作当前实时数据。",
              "Recorded large files are a limited snapshot, not a complete file list. Cached results show the last scan, not a live measurement.",
            )}
          </p>
          <p>
            {tr(
              "统计仅覆盖已读取的路径。APFS 共享块、快照及其他卷可能导致目录合计与整盘已用空间不同。",
              "Figures cover readable paths. APFS sharing, snapshots and other volumes can differ from directory totals.",
            )}
          </p>
          <div className="scan-detail-list">
            {scan?.issues.map((issue, i) => (
              <p key={i}>
                <code>{issue.path}</code>
                <small>{issue.message}</small>
              </p>
            ))}
          </div>
          </DialogBody>
          <DialogFooter>
            <Button variant="outline" disabled={busy} onClick={() => {
              setScanDetailsOpen(false);
              void refresh(currentPath, true, hasProtectedAccess, false, undefined, "full");
            }}>
              {tr("完整重新分析", "Complete rescan")}
            </Button>
            <Button variant="outline" onClick={() => setScanDetailsOpen(false)}>
              {tr("关闭", "Close")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

    </TooltipProvider>
  );
}
