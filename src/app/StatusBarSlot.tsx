import { statusBarSlots, type StatusBarSlotName } from './statusBarSlots';

/** Renders the component registered for `name`, or nothing. */
export function StatusBarSlot({ name }: { name: StatusBarSlotName }) {
  const Slot = statusBarSlots[name];
  return Slot ? <Slot /> : null;
}
