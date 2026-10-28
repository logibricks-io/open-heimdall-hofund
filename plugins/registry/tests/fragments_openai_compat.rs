//! Parity checks for the data-only OpenAI-compatible preset batches.

use hofund_plugin_api::ProtocolId;
use hofund_plugin_registry::{builtin_protocols, find_preset, model_plan_of, select};
use hofund_provider::{Credential, ModelPlan, Resolve, Wire, catalog_ids, credential_for, resolve};

const OVERRIDE: &str = "https://override.example.test/v1";

struct Row {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    catalog: &'static str,
    note: &'static str,
    chat: &'static str,
    responses: &'static str,
    anthropic: &'static str,
    website: &'static str,
    keys_url: &'static str,
}

struct Alias {
    id: &'static str,
    preset: &'static str,
    region: Option<&'static str>,
}

struct Region {
    id: &'static str,
    name: &'static str,
    chat: &'static str,
    responses: &'static str,
    anthropic: &'static str,
    decide: &'static str,
    catalog: &'static str,
    website: &'static str,
    keys_url: &'static str,
    models: Option<&'static [&'static str]>,
    lists: bool,
    embeddings: &'static [&'static str],
    drawers: &'static [&'static str],
    videos: &'static [&'static str],
}

const NO_REGION: Region = Region {
    id: "",
    name: "",
    chat: "",
    responses: "",
    anthropic: "",
    decide: "",
    catalog: "",
    website: "",
    keys_url: "",
    models: None,
    lists: false,
    embeddings: &[],
    drawers: &[],
    videos: &[],
};

#[derive(Clone, Copy)]
struct Details {
    regions: &'static [Region],
    region_label: &'static str,
    hosts: bool,
    key_hint: &'static str,
    no_key: bool,
    short: &'static str,
    no_list: bool,
    decide: &'static str,
    only: &'static str,
    models: &'static [&'static str],
    header_hints: &'static [&'static str],
}

const NO_DETAILS: Details = Details {
    regions: &[],
    region_label: "",
    hosts: false,
    key_hint: "",
    no_key: false,
    short: "",
    no_list: false,
    decide: "",
    only: "",
    models: &[],
    header_hints: &[],
};

const ROWS: &[Row] = &[
    // reference: internal/provider/presets.go:525
    Row {
        id: "302ai",
        name: "302.AI",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://api.302.ai/v1",
        responses: "",
        anthropic: "https://api.302.ai",
        website: "https://302.ai",
        keys_url: "https://302.ai/api-keys/list",
    },
    // reference: internal/provider/presets.go:515
    Row {
        id: "aihubmix",
        name: "AiHubMix",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://aihubmix.com/v1",
        responses: "",
        anthropic: "https://aihubmix.com",
        website: "https://aihubmix.com",
        keys_url: "https://console.aihubmix.com/token",
    },
    // reference: internal/provider/presets.go:528
    Row {
        id: "cherryin",
        name: "CherryIN",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://open.cherryin.ai/v1",
        responses: "https://open.cherryin.ai/v1",
        anthropic: "https://open.cherryin.ai",
        website: "https://open.cherryin.ai",
        keys_url: "https://open.cherryin.ai/console/token",
    },
    // reference: internal/provider/presets.go:486
    Row {
        id: "commandcode",
        name: "Command Code",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://api.commandcode.ai/provider/v1",
        responses: "https://api.commandcode.ai/provider/v1",
        anthropic: "https://api.commandcode.ai/provider",
        website: "https://commandcode.ai/docs/provider",
        keys_url: "https://commandcode.ai/settings/keys",
    },
    // reference: internal/provider/presets.go:492
    Row {
        id: "fireworks",
        name: "Fireworks",
        kind: "relay",
        catalog: "fireworks-ai",
        note: "",
        chat: "https://api.fireworks.ai/inference/v1",
        responses: "",
        anthropic: "",
        website: "https://fireworks.ai",
        keys_url: "https://app.fireworks.ai/settings/users/api-keys",
    },
    // reference: internal/provider/presets.go:511
    Row {
        id: "modelscope",
        name: "ModelScope",
        kind: "relay",
        catalog: "modelscope",
        note: "API-Inference. Not the DashScope endpoints.",
        chat: "https://api-inference.modelscope.cn/v1",
        responses: "https://api-inference.modelscope.cn/v1",
        anthropic: "",
        website: "https://modelscope.cn/docs/model-service/API-Inference/intro",
        keys_url: "https://modelscope.cn/my/myaccesstoken",
    },
    // reference: internal/provider/presets.go:506
    Row {
        id: "nvidia",
        name: "NVIDIA NIM",
        kind: "relay",
        catalog: "nvidia",
        note: "",
        chat: "https://integrate.api.nvidia.com/v1",
        responses: "",
        anthropic: "",
        website: "https://build.nvidia.com",
        keys_url: "https://build.nvidia.com/settings/api-keys",
    },
    // reference: internal/provider/presets.go:449
    Row {
        id: "opencode-go",
        name: "OpenCode Go",
        kind: "relay",
        catalog: "opencode-go",
        note: "open coding models, $10/month",
        chat: "https://opencode.ai/zen/go/v1",
        responses: "https://opencode.ai/zen/go/v1",
        anthropic: "https://opencode.ai/zen/go",
        website: "https://opencode.ai/docs/go",
        keys_url: "https://opencode.ai/auth",
    },
    // reference: internal/provider/presets.go:522
    Row {
        id: "pipellm",
        name: "PipeLLM",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://api.pipellm.ai/openai/v1",
        responses: "https://api.pipellm.ai/v1",
        anthropic: "https://api.pipellm.ai",
        website: "https://www.pipellm.ai",
        keys_url: "https://console.pipellm.ai",
    },
    // reference: internal/provider/presets.go:489
    Row {
        id: "together",
        name: "Together AI",
        kind: "relay",
        catalog: "togetherai",
        note: "",
        chat: "https://api.together.xyz/v1",
        responses: "",
        anthropic: "",
        website: "https://api.together.ai",
        keys_url: "https://api.together.ai/settings/api-keys",
    },
    // reference: internal/provider/presets.go:147
    Row {
        id: "xai",
        name: "xAI",
        kind: "vendor",
        catalog: "xai",
        note: "",
        chat: "https://api.x.ai/v1",
        responses: "https://api.x.ai/v1",
        anthropic: "https://api.x.ai",
        website: "https://console.x.ai",
        keys_url: "https://console.x.ai",
    },
    // reference: internal/provider/presets.go:150
    Row {
        id: "moonshot",
        name: "Kimi",
        kind: "vendor",
        catalog: "moonshotai",
        note: "",
        chat: "https://api.moonshot.ai/v1",
        responses: "",
        anthropic: "https://api.moonshot.ai/anthropic",
        website: "https://platform.moonshot.ai",
        keys_url: "https://platform.moonshot.ai/console/api-keys",
    },
    // reference: internal/provider/presets.go:153
    Row {
        id: "moonshot-cn",
        name: "Kimi (China)",
        kind: "vendor",
        catalog: "moonshotai",
        note: "",
        chat: "https://api.moonshot.cn/v1",
        responses: "",
        anthropic: "https://api.moonshot.cn/anthropic",
        website: "https://platform.moonshot.cn",
        keys_url: "https://platform.moonshot.cn/console/api-keys",
    },
    // reference: internal/provider/presets.go:158
    Row {
        id: "kimi-code",
        name: "Kimi Code",
        kind: "vendor",
        catalog: "kimi-code-plan-global",
        note: "Membership",
        chat: "https://api.kimi.ai/coding/v1",
        responses: "",
        anthropic: "https://api.kimi.ai/coding",
        website: "https://www.kimi.com/code",
        keys_url: "https://www.kimi.com/code/console",
    },
    // reference: internal/provider/presets.go:162
    Row {
        id: "kimi-code-cn",
        name: "Kimi Code (China)",
        kind: "vendor",
        catalog: "kimi-code-plan-cn",
        note: "Membership",
        chat: "https://api.kimi.com/coding/v1",
        responses: "",
        anthropic: "https://api.kimi.com/coding",
        website: "https://www.kimi.com/code",
        keys_url: "https://www.kimi.com/code/console",
    },
    // reference: internal/provider/presets.go:184
    Row {
        id: "minimax",
        name: "MiniMax",
        kind: "vendor",
        catalog: "minimax",
        note: "",
        chat: "https://api.minimax.io/v1",
        responses: "",
        anthropic: "https://api.minimax.io/anthropic",
        website: "https://platform.minimax.io",
        keys_url: "https://platform.minimax.io/user-center/basic-information/interface-key",
    },
    // reference: internal/provider/presets.go:187
    Row {
        id: "minimax-cn",
        name: "MiniMax (China)",
        kind: "vendor",
        catalog: "minimax",
        note: "",
        chat: "https://api.minimaxi.com/v1",
        responses: "",
        anthropic: "https://api.minimaxi.com/anthropic",
        website: "https://platform.minimaxi.com",
        keys_url: "https://platform.minimaxi.com/user-center/basic-information/interface-key",
    },
    // reference: internal/provider/presets.go:333
    Row {
        id: "qwen",
        name: "Qwen",
        kind: "vendor",
        catalog: "alibaba",
        note: "DashScope - intl",
        chat: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        responses: "",
        anthropic: "https://dashscope-intl.aliyuncs.com/apps/anthropic",
        website: "https://modelstudio.console.alibabacloud.com",
        keys_url: "https://modelstudio.console.alibabacloud.com/?tab=playground#/api-key",
    },
    // reference: internal/provider/presets.go:337
    Row {
        id: "qwen-cn",
        name: "Qwen (China)",
        kind: "vendor",
        catalog: "alibaba",
        note: "DashScope - China",
        chat: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        responses: "",
        anthropic: "https://dashscope.aliyuncs.com/apps/anthropic",
        website: "https://bailian.console.aliyun.com",
        keys_url: "https://bailian.console.aliyun.com/?tab=model#/api-key",
    },
    // reference: internal/provider/presets.go:364
    Row {
        id: "mistral",
        name: "Mistral",
        kind: "vendor",
        catalog: "mistral",
        note: "",
        chat: "https://api.mistral.ai/v1",
        responses: "",
        anthropic: "",
        website: "https://console.mistral.ai",
        keys_url: "https://console.mistral.ai/api-keys",
    },
    // reference: internal/provider/presets.go:177
    Row {
        id: "zai",
        name: "Z.ai",
        kind: "vendor",
        catalog: "zhipuai",
        note: "",
        chat: "https://api.z.ai/api/paas/v4",
        responses: "",
        anthropic: "https://api.z.ai/api/anthropic",
        website: "https://z.ai",
        keys_url: "https://z.ai/manage-apikey/apikey-list",
    },
    // reference: internal/provider/presets.go:192
    Row {
        id: "stepfun",
        name: "StepFun",
        kind: "vendor",
        catalog: "stepfun-ai-step-plan, stepfun-ai",
        note: "",
        chat: "https://api.stepfun.ai/step_plan/v1",
        responses: "",
        anthropic: "https://api.stepfun.ai/step_plan",
        website: "https://platform.stepfun.ai",
        keys_url: "https://platform.stepfun.ai/interface-key",
    },
    // reference: internal/provider/presets.go:199
    Row {
        id: "stepfun-cn",
        name: "StepFun (China)",
        kind: "vendor",
        catalog: "stepfun-step-plan, stepfun",
        note: "",
        chat: "https://api.stepfun.com/step_plan/v1",
        responses: "",
        anthropic: "https://api.stepfun.com/step_plan",
        website: "https://platform.stepfun.com",
        keys_url: "https://platform.stepfun.com/interface-key",
    },
    // reference: internal/provider/presets.go:214
    Row {
        id: "xiaomi",
        name: "Xiaomi MiMo",
        kind: "vendor",
        catalog: "xiaomi-token-plan-cn, xiaomi",
        note: "Token Plan - pay as you go",
        chat: "https://token-plan-cn.xiaomimimo.com/v1",
        responses: "https://token-plan-cn.xiaomimimo.com/v1",
        anthropic: "https://token-plan-cn.xiaomimimo.com/anthropic",
        website: "https://platform.xiaomimimo.com",
        keys_url: "https://platform.xiaomimimo.com/token-plan",
    },
    // reference: internal/provider/presets.go:367
    Row {
        id: "groq",
        name: "Groq",
        kind: "vendor",
        catalog: "groq",
        note: "",
        chat: "https://api.groq.com/openai/v1",
        responses: "https://api.groq.com/openai/v1",
        anthropic: "",
        website: "https://console.groq.com",
        keys_url: "https://console.groq.com/keys",
    },
    // reference: internal/provider/presets.go:435
    Row {
        id: "nous",
        name: "Nous Portal",
        kind: "vendor",
        catalog: "",
        note: "Nous Research - Hermes and other models",
        chat: "https://inference-api.nousresearch.com/v1",
        responses: "",
        anthropic: "",
        website: "https://portal.nousresearch.com",
        keys_url: "https://portal.nousresearch.com",
    },
    // reference: internal/provider/presets.go:479
    Row {
        id: "kilo",
        name: "Kilo Gateway",
        kind: "relay",
        catalog: "",
        note: "free models with no key",
        chat: "https://api.kilo.ai/api/openrouter",
        responses: "",
        anthropic: "",
        website: "https://kilo.ai/docs/gateway",
        keys_url: "https://app.kilo.ai",
    },
    // reference: internal/provider/presets.go:495
    Row {
        id: "siliconflow",
        name: "SiliconFlow",
        kind: "relay",
        catalog: "siliconflow",
        note: "",
        chat: "https://api.siliconflow.cn/v1",
        responses: "",
        anthropic: "",
        website: "https://cloud.siliconflow.cn",
        keys_url: "https://cloud.siliconflow.cn/account/ak",
    },
    // reference: internal/provider/presets.go:531
    // name kept ASCII as in the old catalog
    Row {
        id: "yylx",
        name: "Yylx",
        kind: "relay",
        catalog: "",
        note: "",
        chat: "https://app.yylx.io/v1",
        responses: "",
        anthropic: "https://app.yylx.io",
        website: "https://yylx.io",
        keys_url: "https://app.yylx.io/keys",
    },
    // reference: internal/provider/presets.go:295
    // note kept ASCII as in the old catalog
    Row {
        id: "huaweicloud",
        name: "Huawei Cloud MaaS",
        kind: "vendor",
        catalog: "",
        note: "Token Plan, southwest China (Guiyang)",
        chat: "https://api.modelarts-maas.com/plan/v2",
        responses: "",
        anthropic: "https://api.modelarts-maas.com/plan/anthropic",
        website: "https://support.huaweicloud.com/Token-plan-maas/tokenplan-maas-0001.html",
        keys_url: "https://console.huaweicloud.com/modelarts/?#/model-studio/authmanage",
    },
    // reference: internal/provider/presets.go:313
    // note kept ASCII as in the old catalog
    Row {
        id: "volcengine",
        name: "Volcengine Ark",
        kind: "vendor",
        catalog: "",
        note: "Coding plan, agent plan, and pay as you go",
        chat: "https://ark.cn-beijing.volces.com/api/coding/v3",
        responses: "https://ark.cn-beijing.volces.com/api/coding/v3",
        anthropic: "https://ark.cn-beijing.volces.com/api/coding",
        website: "https://www.volcengine.com/docs/82379/1925114",
        keys_url: "https://ark.volcengine.com/region:cn-beijing/apikey",
    },
    // reference: internal/provider/presets.go:349
    // note kept ASCII as in the old catalog
    Row {
        id: "qwen-token-plan",
        name: "Qwen Token Plan",
        kind: "vendor",
        catalog: "",
        note: "Qwen AI platform - subscription",
        chat: "https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1",
        responses: "",
        anthropic: "https://token-plan.maas.qianwenaiapi.com/apps/anthropic",
        website: "https://platform.qianwenai.com/docs/token-plan/overview",
        keys_url: "https://platform.qianwenai.com/home/analytics/token-plan/individual",
    },
    // reference: internal/provider/presets.go:358
    // note kept ASCII as in the old catalog
    Row {
        id: "bailian-token-plan",
        name: "Bailian Token Plan",
        kind: "vendor",
        catalog: "",
        note: "Alibaba Cloud Bailian - subscription",
        chat: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        responses: "",
        anthropic: "https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic",
        website: "https://help.aliyun.com/zh/model-studio/token-plan-overview",
        keys_url: "https://bailian.console.aliyun.com/cn-beijing/subscription/token-plan/personal",
    },
    // reference: internal/provider/presets.go:423
    Row {
        id: "ollama-cloud",
        name: "Ollama Cloud",
        kind: "vendor",
        catalog: "ollama-cloud",
        note: "cloud models, with an API key",
        chat: "https://ollama.com/v1",
        responses: "",
        anthropic: "https://ollama.com",
        website: "https://docs.ollama.com/cloud",
        keys_url: "https://ollama.com/settings/keys",
    },
    // reference: internal/provider/presets.go:457
    Row {
        id: "clinepass",
        name: "ClinePass",
        kind: "relay",
        catalog: "",
        note: "open coding models, $9.99/month",
        chat: "https://api.cline.bot/api/v1",
        responses: "",
        anthropic: "",
        website: "https://docs.cline.bot/getting-started/clinepass",
        keys_url: "https://app.cline.bot",
    },
    // reference: internal/provider/presets.go:465
    Row {
        id: "opencode-zen",
        name: "OpenCode Zen",
        kind: "relay",
        catalog: "opencode",
        note: "",
        chat: "https://opencode.ai/zen/v1",
        responses: "https://opencode.ai/zen/v1",
        anthropic: "https://opencode.ai/zen",
        website: "https://opencode.ai/docs/zen",
        keys_url: "https://opencode.ai/auth",
    },
    // reference: internal/provider/presets.go:231
    // note kept ASCII as in the old catalog
    Row {
        id: "baidu-qianfan",
        name: "Baidu Qianfan",
        kind: "vendor",
        catalog: "",
        note: "Token Plan - pay as you go",
        chat: "https://qianfan.baidubce.com/v2/tokenplan/personal",
        responses: "https://qianfan.baidubce.com/v2/tokenplan/personal",
        anthropic: "https://qianfan.baidubce.com/anthropic/tokenplan/personal",
        website: "https://cloud.baidu.com/doc/qianfan/s/Dmrabu8b6",
        keys_url: "https://console.bce.baidu.com/qianfan/resource/token-plan",
    },
    // reference: internal/provider/presets.go:144
    Row {
        id: "deepseek",
        name: "DeepSeek",
        kind: "vendor",
        catalog: "deepseek",
        note: "",
        chat: "https://api.deepseek.com/v1",
        responses: "https://api.deepseek.com/v1",
        anthropic: "https://api.deepseek.com/anthropic",
        website: "https://platform.deepseek.com",
        keys_url: "https://platform.deepseek.com/api_keys",
    },
    // reference: internal/provider/presets.go:267
    // note and region names kept ASCII as in the old catalog
    Row {
        id: "tencent-cloud",
        name: "Tencent Cloud",
        kind: "vendor",
        catalog: "",
        note: "Token Plan - pay as you go",
        chat: "https://api.lkeap.cloud.tencent.com/plan/v3",
        responses: "",
        anthropic: "https://api.lkeap.cloud.tencent.com/plan/anthropic",
        website: "https://cloud.tencent.com/document/product/1823/130060",
        keys_url: "https://console.cloud.tencent.com/tokenhub/tokenplan",
    },
    // reference: internal/provider/presets.go:166
    Row {
        id: "zhipu",
        name: "Zhipu GLM",
        kind: "vendor",
        catalog: "zhipuai",
        note: "",
        chat: "https://open.bigmodel.cn/api/paas/v4",
        responses: "",
        anthropic: "https://open.bigmodel.cn/api/anthropic",
        website: "https://open.bigmodel.cn",
        keys_url: "https://open.bigmodel.cn/usercenter/proj-mgmt/apikeys",
    },
    // reference: internal/provider/presets.go:542
    // name and note kept ASCII as in the old catalog
    Row {
        id: "typesafe",
        name: "TypeSafe Jev",
        kind: "vendor",
        catalog: "",
        note: "routes groups - picks model and effort",
        chat: "",
        responses: "",
        anthropic: "",
        website: "https://typesafe.ai",
        keys_url: "https://console.typesafe.ai/keys",
    },
    // reference: internal/provider/presets.go:547
    // name and note kept ASCII as in the old catalog
    Row {
        id: "vercel-jev",
        name: "Jev - Vercel AI Gateway",
        kind: "relay",
        catalog: "",
        note: "routes groups - picks model and effort",
        chat: "",
        responses: "",
        anthropic: "",
        website: "https://vercel.com/ai-gateway/models/jev",
        keys_url: "https://vercel.com/d?to=%2F%5Bteam%5D%2F%7E%2Fai-gateway%2Fapi-keys",
    },
    // reference: internal/provider/presets.go:553
    // name and note kept ASCII as in the old catalog
    Row {
        id: "cloudflare-jev",
        name: "Cloudflare Workers AI - Jev, Clef",
        kind: "relay",
        catalog: "",
        note: "routes groups - picks model and effort",
        chat: "",
        responses: "",
        anthropic: "",
        website: "https://developers.cloudflare.com/ai/models/typesafe/jev/",
        keys_url: "https://dash.cloudflare.com/profile/api-tokens",
    },
    // reference: internal/provider/presets.go:560
    // note and region names kept ASCII as in the old catalog
    Row {
        id: "bailian-decision",
        name: "Bailian Decision Model",
        kind: "vendor",
        catalog: "",
        note: "routes groups - picks model and effort",
        chat: "",
        responses: "",
        anthropic: "",
        website: "https://help.aliyun.com/zh/model-studio/decision-model-preview",
        keys_url: "https://bailian.console.aliyun.com/?tab=model#/api-key",
    },
    // reference: internal/provider/presets.go:573
    // note kept ASCII as in the old catalog
    Row {
        id: "qwen-decision",
        name: "Qwen Decision Model",
        kind: "vendor",
        catalog: "",
        note: "routes groups - picks model and effort",
        chat: "",
        responses: "",
        anthropic: "",
        website: "https://www.qianwenai.com/models/decision-model-preview",
        keys_url: "https://platform.qianwenai.com/home/api-keys",
    },
    // reference: internal/provider/presets.go:123
    Row {
        id: "openai",
        name: "OpenAI",
        kind: "vendor",
        catalog: "openai",
        note: "",
        chat: "https://api.openai.com/v1",
        responses: "https://api.openai.com/v1",
        anthropic: "",
        website: "https://platform.openai.com",
        keys_url: "https://platform.openai.com/api-keys",
    },
    // reference: internal/provider/presets.go:440
    Row {
        id: "openrouter",
        name: "OpenRouter",
        kind: "relay",
        catalog: "openrouter",
        note: "",
        chat: "https://openrouter.ai/api/v1",
        responses: "",
        anthropic: "https://openrouter.ai/api",
        website: "https://openrouter.ai",
        keys_url: "https://openrouter.ai/keys",
    },
];

const ALIASES: &[Alias] = &[
    // reference: internal/provider/presets.go:672
    Alias {
        id: "qianfan-token-plan",
        preset: "baidu-qianfan",
        region: None,
    },
    // reference: internal/provider/presets.go:673
    Alias {
        id: "tencent-token-plan",
        preset: "tencent-cloud",
        region: Some("plan"),
    },
    // reference: internal/provider/presets.go:675
    Alias {
        id: "tencent-tokenhub",
        preset: "tencent-cloud",
        region: Some("intl"),
    },
    // reference: internal/provider/presets.go:674
    Alias {
        id: "tencent-tokenhub-cn",
        preset: "tencent-cloud",
        region: Some("cn"),
    },
];

const BAIDU_QIANFAN_MODELS: &[&str] = &[
    "qianfan-code-latest",
    "deepseek-v4.1-flash",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "deepseek-v4-flash",
    "deepseek-v4-flash-0731",
    "deepseek-v3.2",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5",
];

const TENCENT_PLAN_MODELS: &[&str] = &[
    "tc-code-latest",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5",
    "kimi-k3",
    "kimi-k2.7-code",
    "deepseek-v4-pro-202606",
    "deepseek-v4-flash-202605",
    "minimax-m3",
    "minimax-m2.7",
    "hy4-preview",
    "hy3",
];

const TENCENT_ENTERPRISE_MODELS: &[&str] = &[
    "auto",
    "hy4-preview",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "kimi-k2.6",
    "minimax-m3",
    "minimax-m2.7",
    "deepseek-v4.1-flash",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "deepseek-v4-flash-0731",
    "deepseek-v4-pro-202606",
    "deepseek-v4-flash-202605",
    "mimo-v2.6-pro",
    "mimo-v2.6-flash",
];

const TENCENT_ENTERPRISE_INTL_MODELS: &[&str] = &[
    "auto",
    "hy4-preview",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "minimax-m3",
    "deepseek-v4.1-flash",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "deepseek-v4-flash-0731",
    "deepseek-v4-pro-202606",
    "deepseek-v4-flash-202605",
    "mimo-v2.6-pro",
    "mimo-v2.6-flash",
];

const ALIBABA_PLAN_MODELS: &[&str] = &[
    "auto",
    "qwen3.8-max",
    "qwen3.8-flash",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.6-flash",
    "deepseek-v4.1-flash",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "deepseek-v4-flash-0731",
    "glm-5.3",
    "glm-5.2",
];

const CLINEPASS_MODELS: &[&str] = &[
    "cline-pass/glm-5.3",
    "cline-pass/glm-5.3-flash",
    "cline-pass/kimi-k3",
    "cline-pass/deepseek-v4-pro",
    "cline-pass/deepseek-v4.1-flash",
    "cline-pass/mimo-v2.5",
    "cline-pass/mimo-v2.5-pro",
    "cline-pass/minimax-m3",
    "cline-pass/muse-spark-1.3-contributor",
    "cline-pass/qwen3.8-max",
    "cline-pass/qwen3.7-max",
    "cline-pass/qwen3.7-plus",
];

const VOLCENGINE_PLAN_MODELS: &[&str] = &[
    "ark-code-latest",
    "doubao-seed-evolving",
    "doubao-seed-2.1-pro",
    "doubao-seed-2.1-lite",
    "doubao-seed-2.1-turbo",
    "doubao-seed-2.0-mini",
    "minimax-m3",
    "glm-5.3",
    "glm-5.3-flash",
    "deepseek-v4.1-flash",
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "kimi-k2.7-code",
    "kimi-k2.8-preview",
    "kimi-k3",
];

const BATCH_DETAILS: &[(&str, Details)] = &[
    (
        "zai",
        Details {
            regions: &[
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://api.z.ai/api/paas/v4",
                    responses: "",
                    anthropic: "https://api.z.ai/api/anthropic",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "coding",
                    name: "Coding Plan",
                    chat: "https://api.z.ai/api/coding/paas/v4",
                    responses: "https://api.z.ai/api/v1",
                    anthropic: "https://api.z.ai/api/anthropic",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            hosts: false,
            key_hint: "",
            no_key: false,
            ..NO_DETAILS
        },
    ),
    (
        "stepfun",
        Details {
            regions: &[
                Region {
                    id: "plan",
                    name: "Step Plan",
                    chat: "https://api.stepfun.ai/step_plan/v1",
                    responses: "",
                    anthropic: "https://api.stepfun.ai/step_plan",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://api.stepfun.ai/v1",
                    responses: "",
                    anthropic: "https://api.stepfun.ai",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            hosts: false,
            key_hint: "",
            no_key: false,
            ..NO_DETAILS
        },
    ),
    (
        "stepfun-cn",
        Details {
            regions: &[
                Region {
                    id: "plan",
                    name: "Step Plan",
                    chat: "https://api.stepfun.com/step_plan/v1",
                    responses: "",
                    anthropic: "https://api.stepfun.com/step_plan",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://api.stepfun.com/v1",
                    responses: "",
                    anthropic: "https://api.stepfun.com",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            hosts: false,
            key_hint: "",
            no_key: false,
            ..NO_DETAILS
        },
    ),
    (
        "xiaomi",
        Details {
            regions: &[
                Region {
                    id: "plan-cn",
                    name: "Plan - China",
                    chat: "https://token-plan-cn.xiaomimimo.com/v1",
                    responses: "https://token-plan-cn.xiaomimimo.com/v1",
                    anthropic: "https://token-plan-cn.xiaomimimo.com/anthropic",
                    catalog: "xiaomi-token-plan-cn, xiaomi",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "plan-sgp",
                    name: "Plan - Singapore",
                    chat: "https://token-plan-sgp.xiaomimimo.com/v1",
                    responses: "https://token-plan-sgp.xiaomimimo.com/v1",
                    anthropic: "https://token-plan-sgp.xiaomimimo.com/anthropic",
                    catalog: "xiaomi-token-plan-sgp, xiaomi",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "plan-ams",
                    name: "Plan - Europe",
                    chat: "https://token-plan-ams.xiaomimimo.com/v1",
                    responses: "https://token-plan-ams.xiaomimimo.com/v1",
                    anthropic: "https://token-plan-ams.xiaomimimo.com/anthropic",
                    catalog: "xiaomi-token-plan-ams, xiaomi",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://api.xiaomimimo.com/v1",
                    responses: "https://api.xiaomimimo.com/v1",
                    anthropic: "https://api.xiaomimimo.com/anthropic",
                    catalog: "xiaomi",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
            ],
            region_label: "",
            hosts: false,
            key_hint: "",
            no_key: false,
            ..NO_DETAILS
        },
    ),
    (
        "groq",
        Details {
            hosts: true,
            ..NO_DETAILS
        },
    ),
    (
        "nous",
        Details {
            hosts: true,
            ..NO_DETAILS
        },
    ),
    (
        "kilo",
        Details {
            key_hint: "optional: free models need no key",
            no_key: true,
            ..NO_DETAILS
        },
    ),
    (
        "siliconflow",
        Details {
            regions: &[
                Region {
                    id: "cn",
                    name: "China",
                    chat: "https://api.siliconflow.cn/v1",
                    responses: "",
                    anthropic: "",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "intl",
                    name: "Global",
                    chat: "https://api.siliconflow.com/v1",
                    responses: "",
                    anthropic: "",
                    catalog: "",
                    website: "https://cloud.siliconflow.com",
                    keys_url: "https://cloud.siliconflow.com/account/ak",
                    ..NO_REGION
                },
            ],
            ..NO_DETAILS
        },
    ),
    (
        "yylx",
        Details {
            regions: &[
                Region {
                    id: "auto",
                    name: "Auto",
                    chat: "https://app.yylx.io/v1",
                    responses: "",
                    anthropic: "https://app.yylx.io",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "global",
                    name: "Global",
                    chat: "https://global.yylx.io/v1",
                    responses: "",
                    anthropic: "https://global.yylx.io",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
                Region {
                    id: "cn",
                    name: "China Mainland",
                    chat: "https://cn.yylx.io/v1",
                    responses: "",
                    anthropic: "https://cn.yylx.io",
                    catalog: "",
                    website: "",
                    keys_url: "",
                    ..NO_REGION
                },
            ],
            ..NO_DETAILS
        },
    ),
    (
        "huaweicloud",
        Details {
            regions: &[
                Region {
                    id: "plan",
                    name: "Token Plan",
                    chat: "https://api.modelarts-maas.com/plan/v2",
                    anthropic: "https://api.modelarts-maas.com/plan/anthropic",
                    ..NO_REGION
                },
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://api.modelarts-maas.com/openai/v1",
                    anthropic: "https://api.modelarts-maas.com/anthropic",
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            short: "Huawei Cloud",
            models: &[
                "glm-5.3",
                "glm-5.1",
                "kimi-k2.6",
                "deepseek-v4.1-flash",
                "deepseek-v4-flash",
            ],
            ..NO_DETAILS
        },
    ),
    (
        "volcengine",
        Details {
            regions: &[
                Region {
                    id: "coding",
                    name: "Coding Plan",
                    chat: "https://ark.cn-beijing.volces.com/api/coding/v3",
                    responses: "https://ark.cn-beijing.volces.com/api/coding/v3",
                    anthropic: "https://ark.cn-beijing.volces.com/api/coding",
                    models: Some(VOLCENGINE_PLAN_MODELS),
                    embeddings: &["doubao-embedding-vision"],
                    ..NO_REGION
                },
                Region {
                    id: "agent",
                    name: "Agent Plan",
                    chat: "https://ark.cn-beijing.volces.com/api/plan/v3",
                    responses: "https://ark.cn-beijing.volces.com/api/plan/v3",
                    anthropic: "https://ark.cn-beijing.volces.com/api/plan",
                    models: Some(VOLCENGINE_PLAN_MODELS),
                    embeddings: &["doubao-embedding-vision"],
                    drawers: &["doubao-seedream-5-0-pro"],
                    videos: &[
                        "doubao-seedance-2.0",
                        "doubao-seedance-2.0-fast",
                        "doubao-seedance-2.0-mini",
                        "doubao-seedance-2.5",
                    ],
                    ..NO_REGION
                },
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://ark.cn-beijing.volces.com/api/v3",
                    responses: "https://ark.cn-beijing.volces.com/api/v3",
                    lists: true,
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            no_list: true,
            ..NO_DETAILS
        },
    ),
    (
        "qwen-token-plan",
        Details {
            short: "Qwen Plan",
            models: ALIBABA_PLAN_MODELS,
            ..NO_DETAILS
        },
    ),
    (
        "bailian-token-plan",
        Details {
            short: "Bailian Plan",
            decide: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            models: ALIBABA_PLAN_MODELS,
            ..NO_DETAILS
        },
    ),
    (
        "ollama-cloud",
        Details {
            hosts: true,
            decide: "https://ollama.com/v1",
            ..NO_DETAILS
        },
    ),
    (
        "clinepass",
        Details {
            only: "cline-pass/",
            models: CLINEPASS_MODELS,
            ..NO_DETAILS
        },
    ),
    (
        "opencode-zen",
        Details {
            key_hint: "optional: free models need no key",
            no_key: true,
            decide: "https://opencode.ai/zen/v1",
            ..NO_DETAILS
        },
    ),
    (
        "baidu-qianfan",
        Details {
            regions: &[
                Region {
                    id: "personal",
                    name: "Token Plan Personal",
                    chat: "https://qianfan.baidubce.com/v2/tokenplan/personal",
                    responses: "https://qianfan.baidubce.com/v2/tokenplan/personal",
                    anthropic: "https://qianfan.baidubce.com/anthropic/tokenplan/personal",
                    ..NO_REGION
                },
                Region {
                    id: "team",
                    name: "Token Plan Enterprise",
                    chat: "https://qianfan.baidubce.com/v2/tokenplan/team",
                    responses: "https://qianfan.baidubce.com/v2/tokenplan/team",
                    anthropic: "https://qianfan.baidubce.com/anthropic/tokenplan/team",
                    ..NO_REGION
                },
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://qianfan.baidubce.com/v2",
                    responses: "https://qianfan.baidubce.com/v2",
                    anthropic: "https://qianfan.baidubce.com/anthropic",
                    lists: true,
                    keys_url: "https://console.bce.baidu.com/iam/#/iam/apikey/list",
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            no_list: true,
            models: BAIDU_QIANFAN_MODELS,
            ..NO_DETAILS
        },
    ),
    (
        "tencent-cloud",
        Details {
            regions: &[
                Region {
                    id: "plan",
                    name: "Plan - China",
                    chat: "https://api.lkeap.cloud.tencent.com/plan/v3",
                    anthropic: "https://api.lkeap.cloud.tencent.com/plan/anthropic",
                    website: "https://cloud.tencent.com/document/product/1823/130060",
                    keys_url: "https://console.cloud.tencent.com/tokenhub/tokenplan",
                    models: Some(TENCENT_PLAN_MODELS),
                    ..NO_REGION
                },
                Region {
                    id: "cn",
                    name: "Pay as you go - China",
                    chat: "https://tokenhub.tencentmaas.com/v1",
                    responses: "https://tokenhub.tencentmaas.com/v1",
                    anthropic: "https://tokenhub.tencentmaas.com",
                    catalog: "tencent-tokenhub",
                    website: "https://cloud.tencent.com/document/product/1823/130078",
                    keys_url: "https://console.cloud.tencent.com/tokenhub/apikey",
                    ..NO_REGION
                },
                Region {
                    id: "intl",
                    name: "Pay as you go - Global",
                    chat: "https://tokenhub-intl.tencentmaas.com/v1",
                    responses: "https://tokenhub-intl.tencentmaas.com/v1",
                    anthropic: "https://tokenhub-intl.tencentmaas.com",
                    catalog: "tencent-tokenhub",
                    website: "https://www.tencentcloud.com/document/product/1300/78939",
                    keys_url: "https://console.tencentcloud.com/tokenhub/apikey",
                    ..NO_REGION
                },
                Region {
                    id: "enterprise",
                    name: "Enterprise plan - China",
                    chat: "https://tokenhub.tencentmaas.com/plan/v3",
                    responses: "https://tokenhub.tencentmaas.com/plan/v3",
                    anthropic: "https://tokenhub.tencentmaas.com/plan/anthropic",
                    website: "https://cloud.tencent.com/document/product/1823/130659",
                    keys_url: "https://console.cloud.tencent.com/tokenhub/tokenplan",
                    models: Some(TENCENT_ENTERPRISE_MODELS),
                    ..NO_REGION
                },
                Region {
                    id: "enterprise-intl",
                    name: "Enterprise plan - Global",
                    chat: "https://tokenhub-intl.tencentmaas.com/plan/v3",
                    responses: "https://tokenhub-intl.tencentmaas.com/plan/v3",
                    anthropic: "https://tokenhub-intl.tencentmaas.com/plan/anthropic",
                    website: "https://cloud.tencent.com/document/product/1823/130659",
                    keys_url: "https://console.tencentcloud.com/tokenhub/tokenplan",
                    models: Some(TENCENT_ENTERPRISE_INTL_MODELS),
                    ..NO_REGION
                },
            ],
            hosts: true,
            ..NO_DETAILS
        },
    ),
    (
        "zhipu",
        Details {
            regions: &[
                Region {
                    id: "api",
                    name: "Pay as you go",
                    chat: "https://open.bigmodel.cn/api/paas/v4",
                    anthropic: "https://open.bigmodel.cn/api/anthropic",
                    ..NO_REGION
                },
                Region {
                    id: "coding",
                    name: "Coding Plan",
                    chat: "https://open.bigmodel.cn/api/coding/paas/v4",
                    responses: "https://open.bigmodel.cn/api/v1",
                    anthropic: "https://open.bigmodel.cn/api/anthropic",
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            ..NO_DETAILS
        },
    ),
    (
        "typesafe",
        Details {
            decide: "https://api.typesafe.ai/v1",
            ..NO_DETAILS
        },
    ),
    (
        "vercel-jev",
        Details {
            decide: "https://ai-gateway.vercel.sh/typesafe",
            ..NO_DETAILS
        },
    ),
    (
        "cloudflare-jev",
        Details {
            decide: "https://api.cloudflare.com/client/v4",
            ..NO_DETAILS
        },
    ),
    (
        "bailian-decision",
        Details {
            regions: &[
                Region {
                    id: "cn-beijing",
                    name: "Workspace - Beijing",
                    decide: "https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                    ..NO_REGION
                },
                Region {
                    id: "ap-southeast-1",
                    name: "Workspace - Singapore",
                    decide: "https://{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
                    keys_url: "https://modelstudio.console.alibabacloud.com/?tab=playground#/api-key",
                    ..NO_REGION
                },
                Region {
                    id: "token-plan",
                    name: "Token Plan - Beijing",
                    decide: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                    keys_url: "https://bailian.console.aliyun.com/cn-beijing/subscription/token-plan/personal",
                    ..NO_REGION
                },
            ],
            region_label: "Plan",
            short: "Bailian Decision",
            decide: "https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            ..NO_DETAILS
        },
    ),
    (
        "qwen-decision",
        Details {
            short: "Qwen Decision",
            decide: "https://maas.qianwenaiapi.com/compatible-mode/v1",
            ..NO_DETAILS
        },
    ),
    (
        "openrouter",
        Details {
            decide: "https://openrouter.ai/api/v1",
            header_hints: &["HTTP-Referer", "X-OpenRouter-Title"],
            ..NO_DETAILS
        },
    ),
];

fn details(row: &Row) -> &'static Details {
    BATCH_DETAILS
        .iter()
        .find_map(|(id, details)| (*id == row.id).then_some(details))
        .unwrap_or(&NO_DETAILS)
}

fn regions(row: &Row) -> impl Iterator<Item = Option<&'static Region>> {
    std::iter::once(None).chain(details(row).regions.iter().map(Some))
}

fn preset_row(id: &str) -> &'static Row {
    ROWS.iter().find(|row| row.id == id).expect("batch row")
}

fn effective_alias_region(
    alias: &Alias,
    requested: Option<&'static Region>,
) -> Option<&'static Region> {
    requested.or_else(|| {
        let row = preset_row(alias.preset);
        alias
            .region
            .and_then(|id| details(row).regions.iter().find(|region| region.id == id))
    })
}

fn endpoint(row: &Row, region: Option<&Region>, wire: Wire) -> &'static str {
    let regional = match (region, wire) {
        (Some(region), Wire::Chat) => region.chat,
        (Some(region), Wire::Responses) => region.responses,
        (Some(region), Wire::Messages) => region.anthropic,
        _ => "",
    };
    if !regional.is_empty() {
        return regional;
    }
    match wire {
        Wire::Chat => row.chat,
        Wire::Responses => row.responses,
        Wire::Messages => row.anthropic,
        Wire::Generative | Wire::Vertex | Wire::Converse => "",
    }
}

fn decide_only(row: &Row) -> bool {
    row.chat.is_empty()
        && row.responses.is_empty()
        && row.anthropic.is_empty()
        && !details(row).decide.is_empty()
        && details(row).regions.iter().all(|region| {
            region.chat.is_empty() && region.responses.is_empty() && region.anthropic.is_empty()
        })
}

fn decide_endpoint(row: &Row, region: Option<&Region>) -> &'static str {
    region
        .map(|region| region.decide)
        .filter(|decide| !decide.is_empty())
        .unwrap_or(details(row).decide)
}

fn wire_name(wire: Wire) -> &'static str {
    match wire {
        Wire::Chat => "chat",
        Wire::Responses => "responses",
        Wire::Messages => "messages",
        Wire::Generative | Wire::Vertex | Wire::Converse => panic!("not a batch wire"),
    }
}

fn served(row: &Row, region: Option<&Region>) -> impl Iterator<Item = (ProtocolId, Wire)> {
    [
        (ProtocolId::OPENAI_CHAT, Wire::Chat),
        (ProtocolId::OPENAI_RESPONSES, Wire::Responses),
        (ProtocolId::ANTHROPIC_MESSAGES, Wire::Messages),
    ]
    .into_iter()
    .filter(move |(_, wire)| !endpoint(row, region, *wire).is_empty())
}

fn expected_credential(wire: Wire) -> Credential {
    if wire == Wire::Messages {
        Credential::MessagesBoth
    } else {
        Credential::Bearer
    }
}

fn expected_model_plan(row: &Row, region: Option<&Region>) -> ModelPlan {
    let details = details(row);
    ModelPlan {
        chat: endpoint(row, region, Wire::Chat).to_owned(),
        responses: endpoint(row, region, Wire::Responses).to_owned(),
        anthropic: endpoint(row, region, Wire::Messages).to_owned(),
        generative: String::new(),
        no_list: details.no_list,
        lists: region.is_some_and(|region| region.lists),
        only: details.only.to_owned(),
        models: region
            .and_then(|region| region.models)
            .unwrap_or(details.models)
            .iter()
            .map(|model| (*model).to_owned())
            .collect(),
        quirks: None,
        extra_body: None,
    }
}

#[test]
fn batch_rows_live_in_openai_compat_and_left_the_catalog() {
    let catalog = catalog_ids();
    for row in ROWS {
        let (preset, region) = find_preset(row.id).expect("compiled batch row");
        assert_eq!(preset.plugin, "openai-compat");
        assert_eq!(region, None);
        assert!(!catalog.contains(&row.id), "{}", row.id);
        let error = resolve(&Resolve {
            id: row.id,
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect_err("row left the built-in catalog");
        assert_eq!(error.to_string(), format!("unknown preset `{}`", row.id));
    }
    for alias in ALIASES {
        let (preset, region) = find_preset(alias.id).expect("compiled batch alias");
        assert_eq!(preset.id, alias.preset);
        assert_eq!(preset.plugin, "openai-compat");
        assert_eq!(region, alias.region);
        assert!(!catalog.contains(&alias.id), "{}", alias.id);
        let error = resolve(&Resolve {
            id: alias.id,
            region: None,
            wire: None,
            base_url: "",
            project: "",
            location: "",
        })
        .expect_err("alias left the built-in catalog");
        assert_eq!(error.to_string(), format!("unknown preset `{}`", alias.id));
    }
}

#[test]
fn batch_rows_match_the_reference_literal() {
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch row");
        assert_eq!(preset.id, row.id);
        assert_eq!(preset.name, row.name);
        assert_eq!(preset.kind, row.kind);
        assert_eq!(preset.catalog, row.catalog);
        assert_eq!(preset.note, row.note);
        assert_eq!(preset.chat, row.chat);
        assert_eq!(preset.responses, row.responses);
        assert_eq!(preset.anthropic, row.anthropic);
        assert_eq!(preset.website, row.website);
        assert_eq!(preset.keys_url, row.keys_url);
        let details = details(row);
        assert_eq!(preset.region_label, details.region_label);
        assert_eq!(preset.hosts, details.hosts);
        assert_eq!(preset.key_hint, details.key_hint);
        assert_eq!(preset.no_key, details.no_key);
        assert_eq!(preset.short, details.short);
        assert_eq!(preset.no_list, details.no_list);
        assert_eq!(preset.decide, details.decide);
        assert_eq!(preset.only, details.only);
        assert_eq!(preset.models, details.models);
        assert_eq!(preset.header_hints, details.header_hints);
        assert_eq!(preset.regions.len(), details.regions.len());
        for (actual, expected) in preset.regions.iter().zip(details.regions) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.chat, expected.chat);
            assert_eq!(actual.responses, expected.responses);
            assert_eq!(actual.anthropic, expected.anthropic);
            assert_eq!(actual.decide, expected.decide);
            assert_eq!(actual.catalog, expected.catalog);
            assert_eq!(actual.website, expected.website);
            assert_eq!(actual.keys_url, expected.keys_url);
            assert_eq!(actual.models, expected.models);
            assert_eq!(actual.lists, expected.lists);
            assert_eq!(actual.embeddings, expected.embeddings);
            assert_eq!(actual.drawers, expected.drawers);
            assert_eq!(actual.videos, expected.videos);
        }
    }
    for alias in ALIASES {
        let actual = hofund_plugin_registry::compiled_aliases()
            .iter()
            .find(|actual| actual.id == alias.id)
            .expect("compiled batch alias");
        assert_eq!(actual.preset, alias.preset);
        assert_eq!(actual.region, alias.region);
    }
}

#[test]
fn batch_rows_resolve_like_the_old_catalog() {
    let protocols = builtin_protocols();
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch row");
        for region in regions(row) {
            let region_id = region.map(|region| region.id);
            if decide_only(row) {
                for (protocol, wire) in [
                    (None, Wire::Chat),
                    (Some(ProtocolId::OPENAI_CHAT), Wire::Chat),
                    (Some(ProtocolId::OPENAI_RESPONSES), Wire::Responses),
                    (Some(ProtocolId::ANTHROPIC_MESSAGES), Wire::Messages),
                ] {
                    for base_url in ["", OVERRIDE] {
                        let error = select(preset, region_id, &protocols, protocol, base_url)
                            .expect_err("decide-only old catalog resolution");
                        let expected = if protocol.is_none() {
                            format!(
                                "preset `{}` only classifies a route. It has no chat, responses, or messages endpoint.",
                                row.id
                            )
                        } else {
                            format!("preset `{}` has no {} endpoint", row.id, wire_name(wire))
                        };
                        assert_eq!(error.to_string(), expected, "{} {region_id:?}", row.id);
                        let credential_base = if base_url.is_empty() {
                            decide_endpoint(row, region)
                        } else {
                            OVERRIDE
                        };
                        assert_eq!(
                            credential_for(row.id, wire, credential_base),
                            expected_credential(wire),
                            "{} {wire:?} {base_url:?}",
                            row.id
                        );
                    }
                }
                continue;
            }
            let cases = std::iter::once((None, Wire::Chat))
                .chain(served(row, region).map(|(protocol, wire)| (Some(protocol), wire)));
            for (protocol, wire) in cases {
                for base_url in ["", OVERRIDE] {
                    let selected = select(preset, region_id, &protocols, protocol, base_url)
                        .expect("old catalog resolution");
                    let expected_base = if base_url.is_empty() {
                        endpoint(row, region, wire)
                    } else {
                        OVERRIDE
                    };
                    assert_eq!(
                        selected.protocol,
                        protocol.unwrap_or(ProtocolId::OPENAI_CHAT)
                    );
                    assert_eq!(selected.wire, wire, "{}", row.id);
                    assert_eq!(selected.base_url, expected_base, "{}", row.id);
                    assert_eq!(selected.region.map(|region| region.id), region_id);
                    assert_eq!(
                        credential_for(row.id, selected.wire, &selected.base_url),
                        expected_credential(wire),
                        "{} {wire:?} {base_url:?}",
                        row.id
                    );
                }
            }
        }
    }
    for alias in ALIASES {
        let row = preset_row(alias.preset);
        let (preset, _) = find_preset(alias.id).expect("compiled batch alias");
        for requested in regions(row) {
            let region = effective_alias_region(alias, requested);
            let region_id = region.map(|region| region.id);
            let cases = std::iter::once((None, Wire::Chat))
                .chain(served(row, region).map(|(protocol, wire)| (Some(protocol), wire)));
            for (protocol, wire) in cases {
                for base_url in ["", OVERRIDE] {
                    let selected = select(preset, region_id, &protocols, protocol, base_url)
                        .expect("old catalog alias resolution");
                    let expected_base = if base_url.is_empty() {
                        endpoint(row, region, wire)
                    } else {
                        OVERRIDE
                    };
                    assert_eq!(
                        selected.protocol,
                        protocol.unwrap_or(ProtocolId::OPENAI_CHAT)
                    );
                    assert_eq!(selected.wire, wire, "{}", alias.id);
                    assert_eq!(selected.base_url, expected_base, "{}", alias.id);
                    assert_eq!(selected.region.map(|region| region.id), region_id);
                    assert_eq!(
                        credential_for(row.id, selected.wire, &selected.base_url),
                        expected_credential(wire),
                        "{} {wire:?} {base_url:?}",
                        alias.id
                    );
                }
            }
        }
    }
}

#[test]
fn batch_rows_reject_an_unknown_region_like_the_old_catalog() {
    let protocols = builtin_protocols();
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch row");
        let error = select(preset, Some("missing"), &protocols, None, "")
            .expect_err("unknown region must fail");
        let details = details(row);
        if details.regions.is_empty() {
            assert_eq!(
                error.to_string(),
                format!("preset `{}` has no regions", row.id)
            );
        } else {
            let ids = details
                .regions
                .iter()
                .map(|region| region.id)
                .collect::<Vec<_>>();
            let (last, rest) = ids.split_last().expect("nonempty regions");
            assert_eq!(
                error.to_string(),
                format!(
                    "preset `{}` has no region `missing`; expected {}, or {last}",
                    row.id,
                    rest.join(", ")
                )
            );
        }
    }
    for alias in ALIASES {
        let row = preset_row(alias.preset);
        let (preset, _) = find_preset(alias.id).expect("compiled batch alias");
        let error = select(preset, Some("missing"), &protocols, None, "")
            .expect_err("unknown alias region must fail");
        let ids = details(row)
            .regions
            .iter()
            .map(|region| region.id)
            .collect::<Vec<_>>();
        let (last, rest) = ids.split_last().expect("nonempty regions");
        assert_eq!(
            error.to_string(),
            format!(
                "preset `{}` has no region `missing`; expected {}, or {last}",
                row.id,
                rest.join(", ")
            )
        );
    }
}

#[test]
fn batch_rows_model_plan_like_the_old_catalog() {
    for row in ROWS {
        let (preset, _) = find_preset(row.id).expect("compiled batch row");
        for region in regions(row) {
            let compiled_region = region.map(|region| {
                preset
                    .regions
                    .iter()
                    .find(|actual| actual.id == region.id)
                    .expect("compiled region")
            });
            assert_eq!(
                model_plan_of(preset, compiled_region),
                expected_model_plan(row, region)
            );
        }
    }
    for alias in ALIASES {
        let row = preset_row(alias.preset);
        let (preset, _) = find_preset(alias.id).expect("compiled batch alias");
        for requested in regions(row) {
            let region = effective_alias_region(alias, requested);
            let compiled_region = region.map(|region| {
                preset
                    .regions
                    .iter()
                    .find(|actual| actual.id == region.id)
                    .expect("compiled region")
            });
            assert_eq!(
                model_plan_of(preset, compiled_region),
                expected_model_plan(row, region)
            );
        }
    }
}

#[test]
fn moved_decide_only_row_reports_explicit_wire_error() {
    let (preset, _) = find_preset("typesafe").expect("moved decide-only preset");
    let error = select(
        preset,
        None,
        &builtin_protocols(),
        Some(ProtocolId::OPENAI_CHAT),
        OVERRIDE,
    )
    .expect_err("explicit wire on a real decide-only row");
    assert_eq!(error.to_string(), "preset `typesafe` has no chat endpoint");
}

#[test]
fn moved_decide_only_row_preserves_decide_url() {
    let (preset, _) = find_preset("typesafe").expect("moved decide-only preset");
    assert_eq!(preset.decide, "https://api.typesafe.ai/v1");
}

#[test]
fn moved_openai_rows_reject_unserved_wires_like_the_old_catalog() {
    for (id, protocol, expected) in [
        (
            "openai",
            ProtocolId::ANTHROPIC_MESSAGES,
            "preset `openai` has no messages endpoint",
        ),
        (
            "openrouter",
            ProtocolId::OPENAI_RESPONSES,
            "preset `openrouter` has no responses endpoint",
        ),
        (
            "openai",
            ProtocolId::GENERATIVE_LANGUAGE,
            "preset `openai` has no generative endpoint",
        ),
        (
            "openrouter",
            ProtocolId::GENERATIVE_LANGUAGE,
            "preset `openrouter` has no generative endpoint",
        ),
        (
            "openai",
            ProtocolId::BEDROCK_CONVERSE,
            "preset `openai` is served by plugin `openai-compat`, which does not speak `bedrock-converse`; expected openai-chat, openai-responses, anthropic-messages, or generative-language",
        ),
        (
            "openrouter",
            ProtocolId::BEDROCK_CONVERSE,
            "preset `openrouter` is served by plugin `openai-compat`, which does not speak `bedrock-converse`; expected openai-chat, openai-responses, anthropic-messages, or generative-language",
        ),
    ] {
        let (preset, _) = find_preset(id).expect("compiled batch 7 row");
        for base_url in ["", OVERRIDE] {
            let error = select(preset, None, &builtin_protocols(), Some(protocol), base_url)
                .expect_err("unserved wire must fail even with a base override");
            assert_eq!(error.to_string(), expected, "{id} {base_url}");
        }
    }
}
