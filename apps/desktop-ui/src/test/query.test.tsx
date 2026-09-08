import { act, renderHook, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { useQuery } from '../state/useQuery';
import { deferred } from './fixtures';

describe('component query lifetimes', () => {
  it('ignores stale detail/grant replies after changing selection', async () => {
    const first = deferred<string>();
    const second = deferred<string>();
    const oldLoad = () => first.promise;
    const newLoad = () => second.promise;
    const { result, rerender } = renderHook(({ load }) => useQuery(load), { initialProps: { load: oldLoad } });
    rerender({ load: newLoad });
    await act(async () => { second.resolve('current'); });
    await waitFor(() => expect(result.current.data).toBe('current'));
    await act(async () => { first.resolve('stale'); });
    expect(result.current.data).toBe('current');
  });
});
