<script lang="ts">
  import AdjustVertical from '~icons/heroicons/adjustments-vertical';
  import type { DisplayedWebpage } from '$lib/api';
  import { createEventDispatcher } from 'svelte';
  import {
    clearSummary,
    summariesStore,
    markPagesWithAdsStore,
    markPagesWithPaywallStore,
  } from '$lib/stores';
  import Summary from './Summary.svelte';
  import { derived } from 'svelte/store';
  import { improvements } from '$lib/improvements';
  import TextSnippet from '$lib/components/TextSnippet.svelte';
  import StackOverflowSnippet from './StackOverflowSnippet.svelte';

  export let webpage: DisplayedWebpage;
  export let resultIndex: number;

  const summary = derived(summariesStore, ($summaries) => $summaries[webpage.url]);

  let button: HTMLButtonElement;

  const dispatch = createEventDispatcher<{ modal: HTMLButtonElement }>();
</script>

<div class="flex min-w-0 grow flex-col space-y-0.5">
  <div class="flex min-w-0">
    <div class="flex min-w-0 grow flex-col space-y-0.5">
      <div class="flex items-center text-sm">
        <a
          class="text-neutral-focus max-w-[calc(100%-100px)] truncate"
          href={webpage.url}
          use:improvements={resultIndex}
        >
          {webpage.prettyUrl}
        </a>
      </div>
      <a
        class="text-link visited:text-link-visited max-w-[calc(100%-30px)] truncate text-xl font-medium hover:underline"
        title={webpage.title}
        href={webpage.url}
        use:improvements={resultIndex}
      >
        {webpage.title}
      </a>
    </div>
    <button
      class="noscript:hidden text-neutral hover:text-neutral-focus flex w-5 min-w-fit items-center justify-center bg-transparent hover:cursor-pointer"
      bind:this={button}
      on:click|stopPropagation={() => dispatch('modal', button)}
    >
      <AdjustVertical class="text-md" />
    </button>
  </div>
  <div class="text-neutral-focus text-sm font-normal [&>b]:font-bold">
    {#if $summary}
      <Summary url={webpage.url} on:hide={() => clearSummary(webpage)} />
    {:else if webpage.snippet.type == 'normal'}
      <div class="snippet">
        <div class="line-clamp-3">
          <div class="inline">
            <span id="snippet-text" class="snippet-text">
              {#if webpage.likelyHasAds && $markPagesWithAdsStore && webpage.likelyHasPaywall && $markPagesWithPaywallStore}
                <span
                  class="text-neutral border-primary rounded border p-0.5 text-center text-xs"
                  title="page likely has ads and paywall"
                >
                  has ads + paywall
                </span>
              {:else if webpage.likelyHasAds && $markPagesWithAdsStore}
                <span
                  class="text-neutral border-primary rounded border p-0.5 text-center text-xs"
                  title="page likely has ads"
                >
                  has ads
                </span>
              {:else if webpage.likelyHasPaywall && $markPagesWithPaywallStore}
                <span
                  class="text-neutral border-primary rounded border p-0.5 text-center text-xs"
                  title="page likely has paywall"
                >
                  paywall
                </span>
              {/if}
              {#if webpage.snippet.date}
                <span class="text-neutral">
                  {webpage.snippet.date}
                </span> -
              {/if}
              <span>
                <TextSnippet snippet={webpage.snippet.text} />
              </span>
            </span>
          </div>
        </div>
      </div>
    {:else if webpage.snippet.type == 'stackOverflowQA'}
      <div class="snippet">
        <StackOverflowSnippet
          question={webpage.snippet.question}
          answers={webpage.snippet.answers}
        />
      </div>
    {/if}
  </div>
</div>
