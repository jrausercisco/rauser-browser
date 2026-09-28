// The M1 shell uses this narrow subset of Chrome's MV3 APIs. Runtime policy is
// still enforced by the native host; these declarations grant no authority.
interface ChromePort {
  postMessage(message: unknown): void;
  disconnect(): void;
  onMessage: { addListener(listener: (message: unknown) => void): void };
  onDisconnect: { addListener(listener: () => void): void };
}

interface ChromePermissionSet {
  permissions?: string[];
  origins?: string[];
}

interface ChromeTab {
  id?: number;
  windowId?: number;
  url?: string;
  title?: string;
  incognito: boolean;
}

interface ChromeTabActivatedInfo {
  tabId: number;
  windowId: number;
}

interface ChromeTabChangeInfo {
  title?: string;
  url?: string;
  status?: string;
}

interface ChromeNavigationDetails {
  tabId: number;
  frameId: number;
  documentId?: string;
  documentLifecycle?: string;
  url: string;
  timeStamp: number;
}

interface ChromeMessageSender {
  id?: string;
  url?: string;
}

interface ChromeContextMenuClickInfo {
  menuItemId: string | number;
}

interface ChromeApi {
  runtime: {
    id: string;
    getURL(path: string): string;
    reload(): void;
    openOptionsPage(): Promise<void>;
    getManifest(): { options_ui?: { page: string } };
    connectNative(name: string): ChromePort;
    sendMessage<T = unknown>(message: unknown): Promise<T>;
    lastError?: { message: string };
    onMessage: {
      addListener(
        listener: (
          message: unknown,
          sender: ChromeMessageSender,
          sendResponse: (response: unknown) => void,
        ) => boolean | void,
      ): void;
    };
    onInstalled: { addListener(listener: () => void): void };
  };
  sidePanel: {
    setPanelBehavior(options: { openPanelOnActionClick: boolean }): Promise<void>;
    open(options: { tabId?: number; windowId?: number }): Promise<void>;
  };
  commands: {
    onCommand: {
      addListener(listener: (command: string, tab?: ChromeTab) => void): void;
    };
  };
  contextMenus: {
    create(
      properties: { id: string; title: string; contexts: string[] },
      callback?: () => void,
    ): void;
    removeAll(callback?: () => void): void;
    onClicked: {
      addListener(listener: (info: ChromeContextMenuClickInfo, tab?: ChromeTab) => void): void;
    };
  };
  permissions: {
    contains(permissions: ChromePermissionSet): Promise<boolean>;
    request(permissions: ChromePermissionSet): Promise<boolean>;
    remove(permissions: ChromePermissionSet): Promise<boolean>;
    onRemoved: { addListener(listener: (permissions: ChromePermissionSet) => void): void };
    onAdded: { addListener(listener: (permissions: ChromePermissionSet) => void): void };
  };
  storage: {
    local: {
      get(keys: string | string[]): Promise<Record<string, unknown>>;
      set(items: Record<string, unknown>): Promise<void>;
      remove(keys: string | string[]): Promise<void>;
      setAccessLevel(options: { accessLevel: "TRUSTED_CONTEXTS" }): Promise<void>;
    };
    onChanged: {
      addListener(
        listener: (
          changes: Record<string, { oldValue?: unknown; newValue?: unknown }>,
          areaName: string,
        ) => void,
      ): void;
    };
  };
  tabs: {
    get(tabId: number): Promise<ChromeTab>;
    query(queryInfo: { active: boolean; currentWindow: boolean }): Promise<ChromeTab[]>;
    onUpdated: {
      addListener(
        listener: (tabId: number, changeInfo: ChromeTabChangeInfo, tab: ChromeTab) => void,
      ): void;
    };
    onActivated: {
      addListener(listener: (info: ChromeTabActivatedInfo) => void): void;
    };
  };
  webNavigation?: {
    onCommitted: { addListener(listener: (details: ChromeNavigationDetails) => void): void };
    onHistoryStateUpdated: { addListener(listener: (details: ChromeNavigationDetails) => void): void };
  };
}

declare const chrome: ChromeApi;
