import { useEffect, useState } from 'react';
import { errorMessage } from '../api/desktop';

export function useQuery<T>(load: () => Promise<T>) {
  const [revision, setRevision] = useState(0);
  const [result, setResult] = useState<{ data: T | null; error: string | null; loading: boolean }>({ data: null, error: null, loading: true });
  useEffect(() => {
    let active = true;
    setResult({ data: null, error: null, loading: true });
    load().then(data => { if (active) setResult({ data, error: null, loading: false }); },
      error => { if (active) setResult({ data: null, error: errorMessage(error), loading: false }); });
    return () => { active = false; };
  }, [load, revision]);
  return { ...result, reload: () => setRevision(value => value + 1) };
}
