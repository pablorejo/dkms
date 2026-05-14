"use client";

import * as React from "react";
import { AnimatePresence, motion } from "framer-motion";
import { X } from "lucide-react";

import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: React.ReactNode;
  description?: React.ReactNode;
  children?: React.ReactNode;
  footer?: React.ReactNode;
  /** Accessible label when the title is not a string. */
  labelledBy?: string;
  className?: string;
}

const focusableSelector =
  'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])';

/**
 * Lightweight accessible dialog. Replaces window.confirm/window.alert with a
 * semantic ``role="dialog"`` modal that traps focus, restores focus on close,
 * and supports Escape to dismiss.
 *
 * Not backed by Radix to keep the dependency surface small — for most flows
 * in this app (confirmation + simple forms) the custom implementation is
 * enough. For more complex dialogs we can migrate later.
 */
export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  labelledBy,
  className
}: DialogProps) {
  const titleId = React.useId();
  const descriptionId = React.useId();
  const contentRef = React.useRef<HTMLDivElement>(null);
  const previousActiveRef = React.useRef<HTMLElement | null>(null);

  React.useEffect(() => {
    if (!open) return;
    previousActiveRef.current = document.activeElement as HTMLElement | null;

    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";

    // Focus the first focusable element inside the dialog (or the container itself).
    const focusFirst = () => {
      const node = contentRef.current;
      if (!node) return;
      const first = node.querySelector<HTMLElement>(focusableSelector);
      (first ?? node).focus();
    };
    const frame = window.requestAnimationFrame(focusFirst);

    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onOpenChange(false);
        return;
      }
      if (event.key !== "Tab") return;
      const node = contentRef.current;
      if (!node) return;
      const focusable = Array.from(
        node.querySelectorAll<HTMLElement>(focusableSelector)
      ).filter((el) => !el.hasAttribute("disabled"));
      if (focusable.length === 0) {
        event.preventDefault();
        node.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const current = document.activeElement as HTMLElement | null;
      if (event.shiftKey) {
        if (current === first || !node.contains(current)) {
          event.preventDefault();
          last.focus();
        }
      } else if (current === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", handleKey);

    return () => {
      window.cancelAnimationFrame(frame);
      document.body.style.overflow = previousOverflow;
      document.removeEventListener("keydown", handleKey);
      previousActiveRef.current?.focus?.();
    };
  }, [open, onOpenChange]);

  return (
    <AnimatePresence>
      {open ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center p-4 sm:p-6"
          role="presentation"
        >
          <motion.div
            key="overlay"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-background/70 backdrop-blur-sm"
            onClick={() => onOpenChange(false)}
            aria-hidden="true"
          />
          <motion.div
            key="content"
            ref={contentRef}
            tabIndex={-1}
            role="dialog"
            aria-modal="true"
            aria-labelledby={labelledBy ?? titleId}
            aria-describedby={description ? descriptionId : undefined}
            initial={{ opacity: 0, y: 12, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: -8, scale: 0.98 }}
            transition={{ duration: 0.2, ease: "easeOut" }}
            className={cn(
              "relative z-10 w-full max-w-lg overflow-hidden rounded-xl border border-border bg-card text-card-foreground shadow-2xl",
              className
            )}
          >
            <div className="flex items-start gap-4 p-6">
              <div className="flex-1 space-y-1.5">
                <h2 id={titleId} className="text-lg font-semibold leading-tight tracking-tight">
                  {title}
                </h2>
                {description ? (
                  <p id={descriptionId} className="text-sm text-muted-foreground">
                    {description}
                  </p>
                ) : null}
              </div>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="-mr-2 -mt-2 touch-target shrink-0"
                onClick={() => onOpenChange(false)}
                aria-label="Cerrar diálogo"
              >
                <X aria-hidden="true" />
              </Button>
            </div>
            {children ? <div className="px-6 pb-4 text-sm text-foreground">{children}</div> : null}
            {footer ? (
              <div className="flex flex-col-reverse gap-2 border-t border-border bg-muted/40 px-6 py-4 sm:flex-row sm:justify-end">
                {footer}
              </div>
            ) : null}
          </motion.div>
        </div>
      ) : null}
    </AnimatePresence>
  );
}

export interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: React.ReactNode;
  description?: React.ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  destructive?: boolean;
  loading?: boolean;
  onConfirm: () => void | Promise<void>;
}

/**
 * Thin wrapper over Dialog for "yes/cancel" style confirmations. Use this to
 * replace ``window.confirm()`` for destructive actions (delete SAE, revoke
 * certificate, etc.).
 */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel = "Confirmar",
  cancelLabel = "Cancelar",
  destructive = false,
  loading = false,
  onConfirm
}: ConfirmDialogProps) {
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!loading) onOpenChange(next);
      }}
      title={title}
      description={description}
      footer={
        <>
          <Button
            type="button"
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={loading}
          >
            {cancelLabel}
          </Button>
          <Button
            type="button"
            variant={destructive ? "destructive" : "default"}
            onClick={() => {
              void onConfirm();
            }}
            loading={loading}
          >
            {confirmLabel}
          </Button>
        </>
      }
    />
  );
}
