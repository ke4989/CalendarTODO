export function addTag(tags: string[], value: string): string[] {
  const tag = value.trim();
  if (!tag || tags.includes(tag) || tags.length >= 32) return tags;
  return [...tags, tag];
}

export function matchingTags(suggestions: string[], tags: string[], value: string): string[] {
  const prefix = value.trim().toLocaleLowerCase();
  return suggestions.filter((suggestion) => !tags.includes(suggestion) && (!prefix || suggestion.toLocaleLowerCase().startsWith(prefix)));
}

export function collectTagSuggestions(items: Array<{ tags: Array<{ name: string }> }>): string[] {
  return [...new Set(items.flatMap((item) => item.tags.map((tag) => tag.name.trim())).filter(Boolean))]
    .sort((left, right) => left.localeCompare(right, "zh-CN"));
}
