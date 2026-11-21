import type { ComponentProps } from 'react';
import { Button } from './button';
import { Tooltip, TooltipContent, TooltipTrigger } from './tooltip';

type IconButtonProps = ComponentProps<typeof Button> & { label: string };

function IconButton({ label, children, size = 'icon-sm', variant = 'ghost', ...props }: IconButtonProps) {
  return (
    <Tooltip>
      <TooltipTrigger render={<Button {...props} size={size} variant={variant} aria-label={label} />}>{children}</TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

export { IconButton };
