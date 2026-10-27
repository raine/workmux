const repositoryApi = "https://api.github.com/repos/raine/workmux";

export async function getGitHubStars(): Promise<string | null> {
  const headers: Record<string, string> = {
    Accept: "application/vnd.github+json",
    "User-Agent": "workmux-docs-build",
    "X-GitHub-Api-Version": "2022-11-28",
  };
  if (process.env.GITHUB_TOKEN) {
    headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  }

  try {
    const response = await fetch(repositoryApi, {
      headers,
      signal: AbortSignal.timeout(4000),
    });
    if (!response.ok) return null;

    const body: unknown = await response.json();
    if (
      typeof body !== "object" ||
      body === null ||
      !("stargazers_count" in body) ||
      typeof body.stargazers_count !== "number"
    ) {
      return null;
    }

    return new Intl.NumberFormat("en", {
      notation: "compact",
      maximumFractionDigits: 1,
    }).format(body.stargazers_count);
  } catch {
    return null;
  }
}
