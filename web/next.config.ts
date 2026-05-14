import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  basePath: "/web",
  env: {
    NEXT_PUBLIC_BASE_PATH: "/web"
  }
};

export default nextConfig;
