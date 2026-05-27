import type { Metadata, Viewport } from "next";
import { Space_Grotesk, Fraunces } from "next/font/google";
import type { ReactNode } from "react";
import "@/app/globals.css";
import { withBasePath } from "@/lib/app-path";
import { ThemeProvider } from "@/components/theme-provider";

const sans = Space_Grotesk({
  subsets: ["latin"],
  variable: "--font-sans",
  display: "swap"
});

const serif = Fraunces({
  subsets: ["latin"],
  variable: "--font-serif",
  display: "swap"
});

export const metadata: Metadata = {
  title: {
    default: "DKMS Studio",
    template: "%s · DKMS Studio"
  },
  description: "Consola local-first para diseñar, desplegar y monitorizar simulaciones DKMS con enlaces QKD/PQC híbridos.",
  icons: {
    icon: [
      { url: withBasePath("/icon.png"), type: "image/png" },
      { url: withBasePath("/icon.ico"), sizes: "any" }
    ],
    shortcut: withBasePath("/icon.ico"),
    apple: withBasePath("/icon.png")
  }
};

export const viewport: Viewport = {
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#f8f4ec" },
    { media: "(prefers-color-scheme: dark)", color: "#10141b" }
  ],
  width: "device-width",
  initialScale: 1
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="es" suppressHydrationWarning>
      <body
        className={`${sans.variable} ${serif.variable} flex min-h-screen flex-col bg-background font-sans text-foreground antialiased`}
      >
        <ThemeProvider
          attribute="class"
          defaultTheme="system"
          enableSystem
          disableTransitionOnChange
        >
          <a href="#main-content" className="skip-link">
            Saltar al contenido principal
          </a>
          <div className="flex-1">{children}</div>
          <footer className="mt-8 border-t border-border/60 bg-background/70 py-10 backdrop-blur">
            <div className="mx-auto flex max-w-7xl items-center justify-center px-6">
              <img
                src={withBasePath("/logo_retech.png")}
                alt="Retech"
                height={96}
                className="h-24 w-auto opacity-90"
              />
            </div>
          </footer>
        </ThemeProvider>
      </body>
    </html>
  );
}
