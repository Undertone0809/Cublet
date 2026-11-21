import { useRef, type ComponentProps } from 'react';
import { Dialog as DialogPrimitive } from '@base-ui/react/dialog';
import { XIcon } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import '@/mova-controls.css';

function Dialog(props: DialogPrimitive.Root.Props) {
  return <DialogPrimitive.Root data-slot="dialog" {...props} />;
}

function DialogTrigger(props: DialogPrimitive.Trigger.Props) {
  return <DialogPrimitive.Trigger data-slot="dialog-trigger" {...props} />;
}

function DialogPortal(props: DialogPrimitive.Portal.Props) {
  return <DialogPrimitive.Portal data-slot="dialog-portal" {...props} />;
}

function DialogClose(props: DialogPrimitive.Close.Props) {
  return <DialogPrimitive.Close data-slot="dialog-close" {...props} />;
}

function DialogOverlay({ className, ...props }: DialogPrimitive.Backdrop.Props) {
  return (
    <DialogPrimitive.Backdrop
      data-slot="dialog-overlay"
      className={cn('fixed inset-0 isolate z-50 mova-dialog-backdrop bg-black/10 supports-backdrop-filter:backdrop-blur-xs', className)}
      {...props}
    />
  );
}

function DialogContent({ className, children, showCloseButton = true, closeLabel = 'Close', initialFocus, ...props }: DialogPrimitive.Popup.Props & { showCloseButton?: boolean; closeLabel?: string }) {
  const popupRef = useRef<HTMLDivElement>(null);
  return (
    <DialogPortal>
      <DialogOverlay />
      <DialogPrimitive.Popup
        ref={popupRef}
        data-slot="dialog-content"
        className={cn('mova-dialog fixed top-1/2 left-1/2 z-50 grid w-full max-w-[calc(100%-2rem)] -translate-x-1/2 -translate-y-1/2 gap-4 rounded-xl bg-popover p-4 text-sm text-popover-foreground ring-1 ring-foreground/10 outline-none sm:max-w-sm', className)}
        initialFocus={initialFocus ?? (() => popupRef.current?.querySelector<HTMLElement>('[data-slot="dialog-title"]') ?? true)}
        {...props}
      >
        {children}
        {showCloseButton && (
          <DialogPrimitive.Close data-slot="dialog-close" render={<Button variant="ghost" className="absolute top-2 right-2" size="icon-sm" />}>
            <XIcon />
            <span className="sr-only">{closeLabel}</span>
          </DialogPrimitive.Close>
        )}
      </DialogPrimitive.Popup>
    </DialogPortal>
  );
}

function DialogHeader({ className, ...props }: ComponentProps<'div'>) {
  return <div data-slot="dialog-header" className={cn('flex flex-col gap-2', className)} {...props} />;
}

function DialogBody({ className, ...props }: ComponentProps<'div'>) {
  return <div data-slot="dialog-body" className={cn('mova-dialog-body', className)} {...props} />;
}

function DialogFooter({ className, showCloseButton = false, closeLabel = 'Close', children, ...props }: ComponentProps<'div'> & { showCloseButton?: boolean; closeLabel?: string }) {
  return (
    <div data-slot="dialog-footer" className={cn('-mx-4 -mb-4 flex flex-col-reverse gap-2 rounded-b-xl border-t bg-muted/50 p-4 sm:flex-row sm:justify-end', className)} {...props}>
      {children}
      {showCloseButton && <DialogPrimitive.Close render={<Button variant="outline" />}>{closeLabel}</DialogPrimitive.Close>}
    </div>
  );
}

function DialogTitle({ className, ...props }: DialogPrimitive.Title.Props) {
  return <DialogPrimitive.Title data-slot="dialog-title" tabIndex={-1} className={cn('font-heading text-base leading-none font-medium', className)} {...props} />;
}

function DialogDescription({ className, ...props }: DialogPrimitive.Description.Props) {
  return <DialogPrimitive.Description data-slot="dialog-description" className={cn('text-sm text-muted-foreground *:[a]:underline *:[a]:underline-offset-3 *:[a]:hover:text-foreground', className)} {...props} />;
}

export { Dialog, DialogBody, DialogClose, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogOverlay, DialogPortal, DialogTitle, DialogTrigger };
