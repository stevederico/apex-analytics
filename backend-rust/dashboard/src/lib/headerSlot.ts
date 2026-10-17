import { createContext, useContext } from 'react';

// A DOM node in the top header that chart views portal their range/metric
// controls into, so the "date range picker" lives in the navbar. Null when no
// slot is mounted (controls then render inline as a fallback).
export const HeaderSlotContext = createContext<HTMLElement | null>(null);
export const useHeaderSlot = (): HTMLElement | null => useContext(HeaderSlotContext);
