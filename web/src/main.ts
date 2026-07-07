import '@unocss/reset/tailwind.css'
import 'virtual:uno.css'
import { api } from './api'
import { App } from './app'

const root = document.getElementById('app')!

function renderLogin() {
  root.className = 'flex items-center justify-center h-full bg-gray-100'
  root.innerHTML = `
    <form class="bg-white border border-gray-200 rounded-lg shadow-sm p-8 w-96 space-y-4">
      <div class="text-base font-semibold text-gray-800">remote</div>
      <input type="password" placeholder="Password or token" autofocus
        class="w-full border border-gray-200 rounded px-3 py-2 text-base focus:outline-none focus:border-blue-400" />
      <button type="submit"
        class="w-full bg-blue-600 hover:bg-blue-700 text-white rounded py-2 text-base">Sign in</button>
      <div class="text-base text-red-600 hidden" data-err></div>
    </form>`
  const form = root.querySelector('form')!
  const input = form.querySelector('input')!
  const err = form.querySelector('[data-err]') as HTMLElement
  form.onsubmit = async (e) => {
    e.preventDefault()
    err.classList.add('hidden')
    try {
      await api.login(input.value)
      location.reload()
    } catch (ex: any) {
      err.textContent = ex.message
      err.classList.remove('hidden')
    }
  }
}

async function boot() {
  try {
    const me = await api.me()
    new App(me.home, me.hostname).mount(root)
  } catch {
    renderLogin()
  }
}

void boot()
