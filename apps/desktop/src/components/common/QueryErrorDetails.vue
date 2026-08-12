<script setup lang="ts">
import { LocateFixed } from "@lucide/vue";
import { useI18n } from "vue-i18n";
import { Button } from "@/components/ui/button";
import type { SqlErrorPresentation } from "@/lib/sql/sqlDiagnostics";

defineProps<{
  presentation: SqlErrorPresentation;
  canLocate?: boolean;
}>();

const emit = defineEmits<{
  locate: [];
}>();

const { t } = useI18n();
</script>

<template>
  <div class="w-full max-w-2xl space-y-2 text-left text-xs text-foreground">
    <div class="flex items-center justify-between gap-3">
      <span class="font-medium text-destructive">
        {{ t("grid.queryErrorLocation", { line: presentation.line + 1, column: presentation.column + 1 }) }}
      </span>
      <Button v-if="canLocate" variant="outline" size="sm" class="h-7 shrink-0 gap-1.5 px-2 text-xs" @click.stop="emit('locate')">
        <LocateFixed class="h-3.5 w-3.5" />
        {{ t("grid.locateQueryError") }}
      </Button>
    </div>
    <div class="max-w-full overflow-auto rounded-md border bg-muted/20 p-2 font-mono leading-5 select-text">
      <pre class="min-w-max whitespace-pre">{{ presentation.lineText }}</pre>
      <pre class="min-w-max whitespace-pre text-destructive">{{ " ".repeat(presentation.caretColumn) }}^</pre>
    </div>
  </div>
</template>
