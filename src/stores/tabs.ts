import { create } from 'zustand';

export interface TabItem {
  id: string;
  /** i18n key of the tab title. */
  titleKey: string;
  closable: boolean;
}

interface TabsState {
  tabs: TabItem[];
  activeId: string;
  activate: (id: string) => void;
}

export const useTabsStore = create<TabsState>((set) => ({
  tabs: [{ id: 'welcome', titleKey: 'tabs.welcome', closable: false }],
  activeId: 'welcome',
  activate: (id) => set({ activeId: id }),
}));
