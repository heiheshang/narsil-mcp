import { useMemo, useState } from 'react';
import { useTools } from '../hooks/useCodeIntel';

export function ToolsPage() {
  const { data: tools, isLoading, error } = useTools();
  const [query, setQuery] = useState('');
  const [category, setCategory] = useState('All');

  const categories = useMemo(() => {
    const values = new Set<string>();
    for (const tool of tools ?? []) values.add(tool.category);
    return ['All', ...Array.from(values).sort()];
  }, [tools]);

  const filteredTools = useMemo(() => {
    const normalizedQuery = query.trim().toLowerCase();
    return (tools ?? []).filter(tool => {
      const matchesCategory = category === 'All' || tool.category === category;
      const haystack = [
        tool.name,
        tool.description,
        tool.category,
        ...tool.tags,
        ...tool.aliases,
        ...tool.required_flags,
      ].join(' ').toLowerCase();
      return matchesCategory && (!normalizedQuery || haystack.includes(normalizedQuery));
    });
  }, [category, query, tools]);

  return (
    <div className="flex-1 p-8 overflow-auto">
      <div className="max-w-6xl mx-auto">
        <div className="mb-8">
          <h2 className="text-2xl font-bold text-slate-900 dark:text-white mb-2">Tools</h2>
          <p className="text-sm text-slate-500 dark:text-slate-400">
            Browse available narsil tools, their categories, requirements, and input schemas.
          </p>
        </div>

        <div className="flex flex-col sm:flex-row gap-3 mb-6">
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search by name, tag, alias, or flag"
            className="flex-1 h-10 px-3 rounded-lg border border-slate-200 dark:border-slate-700 bg-white dark:bg-slate-900 text-sm text-slate-900 dark:text-white"
          />
          <select
            value={category}
            onChange={(e) => setCategory(e.target.value)}
            className="h-10 px-3 rounded-lg border border-slate-200 dark:border-slate-700 bg-white dark:bg-slate-900 text-sm text-slate-900 dark:text-white"
          >
            {categories.map(value => (
              <option key={value} value={value}>{value}</option>
            ))}
          </select>
        </div>

        {isLoading ? (
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
            {[1, 2, 3, 4].map(i => (
              <div key={i} className="h-56 rounded-xl bg-slate-100 dark:bg-slate-800 animate-pulse" />
            ))}
          </div>
        ) : error ? (
          <div className="rounded-xl border border-red-200 dark:border-red-900 bg-red-50 dark:bg-red-950 px-4 py-3">
            <p className="text-sm text-red-600 dark:text-red-300">Failed to load tools.</p>
            <p className="text-xs text-red-500 dark:text-red-400 mt-1">{String(error)}</p>
          </div>
        ) : (
          <>
            <div className="text-xs text-slate-400 dark:text-slate-500 mb-4">
              Showing {filteredTools.length} of {tools?.length ?? 0} tools
            </div>
            <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
              {filteredTools.map(tool => (
                <section
                  key={tool.name}
                  className="rounded-xl border border-slate-200 dark:border-slate-800 bg-white dark:bg-slate-900 p-5"
                >
                  <div className="flex items-start justify-between gap-3 mb-3">
                    <div>
                      <h3 className="text-sm font-semibold text-slate-900 dark:text-white">{tool.name}</h3>
                      <p className="text-xs text-slate-500 dark:text-slate-400 mt-1">{tool.description}</p>
                    </div>
                    <span className="px-2 py-1 rounded-full bg-slate-100 dark:bg-slate-800 text-[10px] font-medium text-slate-600 dark:text-slate-300 uppercase">
                      {tool.category}
                    </span>
                  </div>

                  <div className="flex flex-wrap gap-2 mb-3 text-[11px]">
                    <span className="px-2 py-1 rounded bg-blue-50 dark:bg-blue-950 text-blue-600 dark:text-blue-300">
                      {tool.stability}
                    </span>
                    <span className="px-2 py-1 rounded bg-amber-50 dark:bg-amber-950 text-amber-600 dark:text-amber-300">
                      {tool.performance}
                    </span>
                    {tool.requires_api_key && (
                      <span className="px-2 py-1 rounded bg-purple-50 dark:bg-purple-950 text-purple-600 dark:text-purple-300">
                        API key
                      </span>
                    )}
                  </div>

                  {tool.required_flags.length > 0 && (
                    <p className="text-xs text-slate-500 dark:text-slate-400 mb-2">
                      Requires: {tool.required_flags.join(', ')}
                    </p>
                  )}

                  {tool.tags.length > 0 && (
                    <p className="text-xs text-slate-500 dark:text-slate-400 mb-2">
                      Tags: {tool.tags.join(', ')}
                    </p>
                  )}

                  {tool.aliases.length > 0 && (
                    <p className="text-xs text-slate-500 dark:text-slate-400 mb-3">
                      Aliases: {tool.aliases.join(', ')}
                    </p>
                  )}

                  <div className="rounded-lg bg-slate-50 dark:bg-slate-950 border border-slate-200 dark:border-slate-800 p-3">
                    <p className="text-[11px] font-medium text-slate-600 dark:text-slate-300 mb-2">
                      Input schema
                    </p>
                    <pre className="text-[11px] leading-5 text-slate-700 dark:text-slate-300 overflow-x-auto whitespace-pre-wrap">
                      {JSON.stringify(tool.input_schema, null, 2)}
                    </pre>
                  </div>
                </section>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
