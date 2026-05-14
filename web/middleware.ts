import type { NextRequest } from "next/server";
import { NextResponse } from "next/server";
import { ORCH_TOKEN_COOKIE_NAME } from "@/lib/auth/constants";
import { stripBasePath, withBasePath } from "@/lib/app-path";

const PROTECTED_PATHS = ["/simulations"];
const AUTH_PAGES = ["/login", "/register"];

function isProtectedPath(pathname: string): boolean {
  return PROTECTED_PATHS.some((path) => pathname === path || pathname.startsWith(`${path}/`));
}

export function middleware(request: NextRequest) {
  const { pathname } = request.nextUrl;
  const normalizedPathname = stripBasePath(pathname);
  const hasSession = Boolean(request.cookies.get(ORCH_TOKEN_COOKIE_NAME)?.value);

  if (isProtectedPath(normalizedPathname) && !hasSession) {
    const loginUrl = new URL(withBasePath("/login"), request.url);
    loginUrl.searchParams.set("next", normalizedPathname);
    return NextResponse.redirect(loginUrl);
  }

  if (AUTH_PAGES.includes(normalizedPathname) && hasSession) {
    return NextResponse.redirect(new URL(withBasePath("/simulations"), request.url));
  }

  return NextResponse.next();
}

export const config = {
  matcher: ["/login", "/register", "/simulations/:path*", "/web/login", "/web/register", "/web/simulations/:path*"]
};
