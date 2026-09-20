import createClient from "openapi-fetch";
import type { paths } from "./schema";

const client = createClient<paths>({
  // 同源相对路径：开发/预览环境由 Vite proxy 转发到后端
  baseUrl: "",
});

export default client;
